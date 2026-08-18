use crate::config::paths::Paths;
use crate::subprocess::git_command;
use crate::utils::bytes_to_hex;
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Short id of a checkpoint commit (`refs/kaji/<id>` in the store). Not a
/// full sha — collisions are theoretically possible but astronomically
/// unlikely within a single project's checkpoint history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointId(pub String);

/// Git-bare-backed store of working-tree snapshots for one project.
///
/// Every operation goes through `--git-dir=<store>` with `work_tree` as
/// `--work-tree`, so the store never touches the user's own `.git` (if
/// any).
pub struct CheckpointStore {
    git_dir: PathBuf,
    /// The project's git toplevel (or the project path itself, when it
    /// isn't inside a git repo), resolved once in `for_project` — see
    /// `resolve_work_tree`. Every git operation this store runs uses this,
    /// never a path supplied later by a caller: `project_key` hashes this
    /// exact value, so the on-disk store this instance resolves to and the
    /// tree every snapshot/restore actually operates on can never diverge,
    /// even when `for_project` was originally called with a subdirectory of
    /// a larger repo.
    work_tree: PathBuf,
    /// premortem PM6: a bare repo has a single on-disk index. Concurrent
    /// `git` invocations against the same `--git-dir` (snapshot racing a
    /// restore, or two snapshots) fight over `index.lock` and can corrupt
    /// it. Two layers serialize:
    ///
    /// - `lock: Mutex<()>` covers *intra-process* callers — a future
    ///   concurrent caller (gateway/ACP) must not be able to bypass it.
    ///   `restore` holds it for its *entire* compound operation (tree
    ///   lookup, created-files diff, read-tree, checkout-index, cleanup) —
    ///   see the `*_locked` helpers below — not once per sub-step, so a
    ///   concurrent `snapshot` can never interleave with a restore in
    ///   progress.
    ///
    /// - `acquire_store_lock` adds an advisory exclusive flock — non-blocking,
    ///   *refusing loudly* rather than waiting — to cover *inter-process*
    ///   callers (a second kaji session on the same project): the Mutex is
    ///   per-instance, so it can't serialize two processes sharing this
    ///   `git_dir`. Held for the same compound window as the Mutex.
    lock: Mutex<()>,
}

/// Resolves the effective work-tree for `project`: the git toplevel when
/// `project` sits inside a git repo, else `project` itself — canonicalized
/// either way. `project_key` hashes exactly this value, so the store's
/// identity key and the `--work-tree` every git command in this file uses
/// are always computed from the same resolved path, even when `project` is
/// a subdirectory of a larger repo.
fn resolve_work_tree(project: &Path) -> PathBuf {
    let base = git_toplevel(project).unwrap_or_else(|| project.to_path_buf());
    std::fs::canonicalize(&base).unwrap_or(base)
}

/// Identity key for a project's on-disk checkpoint store (`<key>.git` under
/// `kaji/checkpoints/`). FIGÉE (premortem PM7): changing this hash changes
/// every project's key, orphaning all existing stores silently (no error —
/// they just look empty). Never modify without a migration path for users'
/// existing stores.
///
/// Deliberately pure (no filesystem access): `resolve_work_tree` owns all
/// path resolution (git toplevel lookup, canonicalize), so this function's
/// output for a given input never depends on canonicalize/symlink behavior
/// at call time — see `project_key_is_stable_for_a_known_path`, which used
/// to be flaky on macOS (`/tmp` → `/private/tmp`) before this split.
fn project_key(work_tree: &Path) -> String {
    let digest = Sha256::digest(work_tree.to_string_lossy().as_bytes());
    bytes_to_hex(digest).chars().take(16).collect()
}

/// Why a directory must not be snapshotted. See [`eligibility`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointIneligible {
    NotAGitWorkTree,
    HomeDirectory,
    FilesystemRoot,
}

impl CheckpointIneligible {
    /// Short user-facing wording, surfaced by the TUI next to
    /// `checkpoints désactivés`.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotAGitWorkTree => "hors dépôt git",
            Self::HomeDirectory => "répertoire home",
            Self::FilesystemRoot => "racine du système",
        }
    }
}

/// Whether `dir` may be used as a checkpoint work tree.
///
/// Every snapshot starts with `git add -A` over the whole work tree, awaited
/// before the turn's first token. Outside a git repo there is no `.gitignore`
/// to prune the walk, and on `~` or `/` the walk covers everything the user
/// owns: a session started from the home directory spent 58 s staging files
/// before each turn could begin. Checkpointing is a recoverability nicety, so
/// these directories lose it rather than paying that cost.
///
/// `dir` is checked before its git toplevel so that a home directory which is
/// itself a repo (dotfiles) reports `HomeDirectory`, and so that `/` reports
/// `FilesystemRoot` instead of the incidental `NotAGitWorkTree`. Both sides of
/// every comparison are canonicalized: on macOS a tempdir resolves through
/// `/private`, and `$HOME` may be a symlink.
pub fn eligibility(dir: &Path, home: Option<&Path>) -> Result<(), CheckpointIneligible> {
    let home = home.map(canonical);
    let dir = canonical(dir);
    let check = |path: &Path| {
        if path.parent().is_none() {
            return Err(CheckpointIneligible::FilesystemRoot);
        }
        if home.as_deref() == Some(path) {
            return Err(CheckpointIneligible::HomeDirectory);
        }
        Ok(())
    };

    check(&dir)?;
    let toplevel = git_toplevel(&dir).ok_or(CheckpointIneligible::NotAGitWorkTree)?;
    check(&canonical(&toplevel))
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn git_toplevel(project: &Path) -> Option<PathBuf> {
    let output = git_command()
        .arg("-C")
        .arg(project)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

fn init_bare(git_dir: &Path) -> Result<()> {
    let output = git_command()
        .args(["init", "--bare", "--quiet"])
        .arg(git_dir)
        .output()
        .context("spawning git init --bare")?;
    if !output.status.success() {
        bail!(
            "git init --bare failed for {}: {}",
            git_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Inter-process serialization for one store (see `CheckpointStore::lock`'s
/// doc comment): an advisory exclusive flock on `<git_dir>/kaji-store.lock`,
/// acquired NON-blocking. A held lock means another kaji process is
/// mid-operation on the same store (a snapshot or — worse — a compound
/// restore) — refuse loudly instead of waiting: an interleaved `add -A` /
/// `read-tree` / `checkout-index` sequence across processes is precisely the
/// index corruption premortem PM6 exists to prevent. The returned `File`
/// keeps the lock while it lives; dropping it releases it.
///
/// Retries a few times on `EWOULDBLOCK` before refusing: macOS releases an
/// `flock` asynchronously, so right after a lock is dropped a fresh open +
/// flock on the same path can spuriously report busy for ~1 ms even though
/// nothing holds it (F_GETLK then names the current process itself,
/// `l_pid = -1`; the lag is amplified by concurrent flock churn in parallel
/// test runs). A store that is *really* held by another process stays busy
/// for tens of ms, so the bounded retry absorbs the ghost without masking
/// genuine contention — a real competitor is still refused loudly.
fn acquire_store_lock(git_dir: &Path) -> Result<File> {
    let path = git_dir.join("kaji-store.lock");
    let mut attempts = 0;
    loop {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("ouverture de {}", path.display()))?;
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                attempts += 1;
                if attempts >= 4 {
                    bail!(
                        "store occupé par une autre instance kaji (snapshot ou restore en cours) — opération refusée, réessaye dans un instant"
                    )
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("flock sur {}", path.display()))
            }
        }
    }
}

fn run_git<I, S>(git_dir: &Path, work_tree: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<std::ffi::OsString> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let output = git_command()
        .arg(format!("--git-dir={}", git_dir.display()))
        .arg(format!("--work-tree={}", work_tree.display()))
        .args(&args)
        .output()
        .context("spawning git")?;
    if !output.status.success() {
        bail!(
            "git {:?} failed (exit {:?}): {}",
            args,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Splits `-z`-terminated git output (`ls-tree -z`, `diff -z`) into
/// non-empty relative paths. Using `-z` instead of the default
/// newline/quoted format sidesteps git's `core.quotePath` octal-escaping of
/// non-ASCII bytes (e.g. `café.txt` → `"caf\303\251.txt"`), which otherwise
/// makes the returned paths not match any real file on disk.
fn split_nul_separated_paths(out: &str) -> Vec<PathBuf> {
    out.split('\0')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Path of one `ls-tree -r -z` entry (`<mode> SP <type> SP <sha> TAB
/// <path>`), or `None` when the entry is not a blob — in practice a gitlink
/// (`160000 commit`), which names a nested git repository the store never
/// captures and `checkout-index` never writes. See `tree_paths_locked`.
fn parse_ls_tree_blob_path(entry: &str) -> Option<PathBuf> {
    let (meta, path) = entry.split_once('\t')?;
    let object_type = meta.split(' ').nth(1)?;
    (object_type == "blob").then(|| PathBuf::from(path))
}

/// Regex-equivalent check for `^[0-9a-f]{6,40}$` — the shape of every id
/// `CheckpointStore` itself ever generates (`snapshot`'s `commit.chars().take(12)`).
/// `restore`'s `target` ultimately comes from user input (`/restore <id>` in
/// the TUI), so this runs before any of it reaches a git subprocess: without
/// it, a string like `--upload-pack=x` embedded into `refs/kaji/<id>` relies
/// entirely on git's own ref-name parsing to fail safely, which is the wrong
/// thing for this code to depend on.
fn validate_checkpoint_id(id: &str) -> Result<()> {
    let well_formed = (6..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !well_formed {
        bail!("id de checkpoint invalide");
    }
    Ok(())
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// premortem-driven safety check (Fix A1): before `checkout-index -f -a`
/// runs, finds every path where `target`'s tree and the live work tree
/// disagree on file-vs-directory. `checkout-index` resolves such a
/// disagreement by deleting whichever side is in its way — recursively, for
/// a directory — to make room for the other. An ordinary refactor
/// (`rm P; mkdir P; ...`) can turn a checkpointed file into a directory; if
/// that directory holds a gitignored secret (`.env`, etc.), checkout-index
/// silently destroys it, because it is invisible to every `add -A` this
/// store ever ran and so never appears in `created` (see
/// `files_created_since`).
///
/// `created` is the same created-since-target list `restore` already
/// computes for its own cleanup pass — anything sitting on disk under a
/// conflicting path that ISN'T in it is content this store cannot account
/// for. Refuses rather than proceeds whenever that happens: the whole point
/// is to never let `checkout-index` be the thing that decides what's safe
/// to delete.
fn refuse_on_destructive_type_conflict(
    work_tree: &Path,
    target_paths: &[PathBuf],
    created: &[PathBuf],
) -> Result<()> {
    let created: HashSet<&Path> = created.iter().map(PathBuf::as_path).collect();

    let mut target_dirs: HashSet<&Path> = HashSet::new();
    for path in target_paths {
        for ancestor in path.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            target_dirs.insert(ancestor);
        }
    }

    // Case 1: target wants a FILE at `path`, but the work tree currently
    // has a DIRECTORY there — checkout-index would recursively remove it.
    for path in target_paths {
        let on_disk = work_tree.join(path);
        if !on_disk.is_dir() {
            continue;
        }
        let mut nested = Vec::new();
        walk_files(&on_disk, &mut nested);
        for file in &nested {
            let Ok(relative) = file.strip_prefix(work_tree) else {
                continue;
            };
            if !created.contains(relative) {
                bail!(
                    "restore refusé : le chemin {} est un dossier avec des fichiers non-suivis que le restore écraserait",
                    path.display()
                );
            }
        }
    }

    // Case 2: target wants a DIRECTORY at `dir` (it has entries under
    // `dir/...`), but the work tree currently has a plain FILE there —
    // checkout-index would remove that file to make room.
    for dir in &target_dirs {
        let on_disk = work_tree.join(dir);
        if !on_disk.is_file() {
            continue;
        }
        if !created.contains(*dir) {
            bail!(
                "restore refusé : le chemin {} est un fichier non-suivi que le restore écraserait pour créer un dossier",
                dir.display()
            );
        }
    }

    Ok(())
}

impl CheckpointStore {
    pub fn for_project(project: &Path) -> Result<Self> {
        let work_tree = resolve_work_tree(project);
        let git_dir =
            Paths::in_data_dir("kaji/checkpoints").join(format!("{}.git", project_key(&work_tree)));
        if !git_dir.exists() {
            std::fs::create_dir_all(&git_dir)
                .with_context(|| format!("creating {}", git_dir.display()))?;
            init_bare(&git_dir)?;
        }
        Ok(Self {
            git_dir,
            work_tree,
            lock: Mutex::new(()),
        })
    }

    /// Snapshots the working tree and returns `(checkpoint id, tree sha)`.
    /// Chains onto `refs/kaji/latest` as parent when one exists, so the
    /// store's commit history is a linear timeline of snapshots.
    pub fn snapshot(&self, label: &str) -> Result<(CheckpointId, String)> {
        let _guard = self.lock.lock().unwrap();
        let _store_lock = acquire_store_lock(&self.git_dir)?;
        run_git(&self.git_dir, &self.work_tree, ["add", "-A"])?;
        let tree = run_git(&self.git_dir, &self.work_tree, ["write-tree"])?
            .trim()
            .to_string();
        let parent = run_git(
            &self.git_dir,
            &self.work_tree,
            ["rev-parse", "--verify", "-q", "refs/kaji/latest"],
        )
        .ok()
        .map(|s| s.trim().to_string());

        let mut args: Vec<String> = vec![
            "commit-tree".into(),
            tree.clone(),
            "-m".into(),
            label.into(),
        ];
        if let Some(parent) = parent {
            args.push("-p".into());
            args.push(parent);
        }
        let commit = run_git(&self.git_dir, &self.work_tree, &args)?
            .trim()
            .to_string();
        let id: String = commit.chars().take(12).collect();
        run_git(
            &self.git_dir,
            &self.work_tree,
            ["update-ref", &format!("refs/kaji/{id}"), &commit],
        )?;
        run_git(
            &self.git_dir,
            &self.work_tree,
            ["update-ref", "refs/kaji/latest", &commit],
        )?;
        Ok((CheckpointId(id), tree))
    }

    /// Lists paths present in the current working tree but absent from
    /// `target_tree` — i.e. files created since that snapshot. Named and
    /// tested in isolation on purpose (premortem PM5): `restore` depends on
    /// it to know exactly which files it is allowed to delete, so its
    /// removal must be a visible, deliberate act, not an inlined detail.
    pub fn files_created_since(&self, target_tree: &str) -> Result<Vec<PathBuf>> {
        let _guard = self.lock.lock().unwrap();
        let _store_lock = acquire_store_lock(&self.git_dir)?;
        self.files_created_since_locked(target_tree)
    }

    /// Same as `files_created_since`, assuming the caller already holds
    /// `self.lock` — see `restore`, which composes this with `tree_of_locked`
    /// and its own read-tree/checkout-index under a single guard (Fix A3).
    fn files_created_since_locked(&self, target_tree: &str) -> Result<Vec<PathBuf>> {
        run_git(&self.git_dir, &self.work_tree, ["add", "-A"])?;
        let current = run_git(&self.git_dir, &self.work_tree, ["write-tree"])?
            .trim()
            .to_string();
        let out = run_git(
            &self.git_dir,
            &self.work_tree,
            [
                // `--no-renames` is load-bearing, not a micro-optimization:
                // with rename detection on (git's default since 2.9), a
                // renamed file is reported as `R100 old new` and emits no
                // `A` line at all, so the restore left the rename target in
                // place while recreating the original — a duplicate, on a
                // tree it then announced as aligned. See
                // `files_created_since_sees_a_renamed_file_as_created`.
                "diff",
                "--no-renames",
                "-z",
                "--name-only",
                "--diff-filter=A",
                target_tree,
                &current,
            ],
        )?;
        Ok(split_nul_separated_paths(&out))
    }

    /// Flat list of every file path in `tree` (git blobs only — no
    /// directory entries), used by `refuse_on_destructive_type_conflict` to
    /// find file/directory disagreements between `tree` and the live work
    /// tree. Assumes `self.lock` is already held.
    ///
    /// Gitlinks (`160000 commit`, i.e. a nested git repository that `add -A`
    /// recorded as a submodule-style entry) are excluded: `ls-tree
    /// --name-only` renders them as a plain path, which the type-conflict
    /// check would then read as "target wants a file, disk has a directory"
    /// and refuse every restore in the project. `checkout-index` never
    /// materializes a gitlink either, so these entries name nothing the
    /// restore will write. See
    /// `restore_ignores_nested_git_repository_gitlinks`.
    fn tree_paths_locked(&self, tree: &str) -> Result<Vec<PathBuf>> {
        let out = run_git(
            &self.git_dir,
            &self.work_tree,
            ["ls-tree", "-r", "-z", tree],
        )?;
        Ok(out
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .filter_map(parse_ls_tree_blob_path)
            .collect())
    }

    /// Assumes `self.lock` is already held — see `restore`.
    fn tree_of_locked(&self, id: &CheckpointId) -> Result<String> {
        let checkpoint_ref = format!("refs/kaji/{}", id.0);
        let out = run_git(
            &self.git_dir,
            &self.work_tree,
            ["rev-parse", &format!("{checkpoint_ref}^{{tree}}")],
        )?;
        Ok(out.trim().to_string())
    }

    /// Restores the working tree to the state captured by `target`.
    ///
    /// premortem PM5: this NEVER runs `git clean`. `--work-tree` is the
    /// project's real work tree, which routinely holds untracked files no
    /// kaji snapshot ever captured (`.env`, `node_modules`, scratch files)
    /// — `git clean -fd` would delete all of them indiscriminately.
    /// Instead, only the paths this store can prove were created strictly
    /// after `target` (via `files_created_since`) are removed. See
    /// `restore_preserves_untracked_files_outside_the_snapshot`, which fails
    /// under `git clean -fdx`.
    ///
    /// Known boundary: `files_created_since` re-stages the live work tree
    /// with `add -A`, which — like any git command — silently skips paths
    /// matched by the project's own `.gitignore`. That is what actually
    /// protects a never-tracked `.env`/`node_modules`/etc: it is invisible
    /// to every `add -A` this store ever runs, in every snapshot, so it
    /// never shows up as "added" relative to any target. A foreign file
    /// that is NOT gitignored and happens to be created inside the exact
    /// window between `target` and this restore call is indistinguishable
    /// from a file the current turn created — there is no provenance
    /// tracking at this layer (out of scope for the store; see spec
    /// non-goals). Untracked files that already existed before the
    /// project's *first* checkpoint are safe regardless, since that first
    /// `add -A` absorbs them into every subsequent tree.
    ///
    /// Fix A1: before any of the above runs, `refuse_on_destructive_type_conflict`
    /// checks for paths where `target`'s tree wants a file and the live
    /// work tree has a directory there (or vice versa). `checkout-index -f
    /// -a` resolves such a conflict by deleting whichever side is in its
    /// way — recursively for a directory, silently destroying anything
    /// nested inside it that `files_created_since` never saw (a gitignored
    /// secret, for instance). When that would happen, `restore` returns
    /// `Err` instead of running `checkout-index` at all — see
    /// `restore_refuses_file_to_directory_type_conflict_with_untracked_content`.
    ///
    /// Fix A3: the whole sequence below — tree lookup, created-files diff,
    /// read-tree, checkout-index, cleanup — runs under one `self.lock`
    /// acquisition, not three, so a concurrent `snapshot` can never observe
    /// or mutate the store mid-restore.
    pub fn restore(&self, target: &CheckpointId) -> Result<()> {
        validate_checkpoint_id(&target.0)?;
        let _guard = self.lock.lock().unwrap();
        let _store_lock = acquire_store_lock(&self.git_dir)?;
        let tree = self.tree_of_locked(target)?;
        let created = self.files_created_since_locked(&tree)?;
        let target_paths = self.tree_paths_locked(&tree)?;
        refuse_on_destructive_type_conflict(&self.work_tree, &target_paths, &created)?;

        let checkpoint_ref = format!("refs/kaji/{}", target.0);
        run_git(
            &self.git_dir,
            &self.work_tree,
            ["read-tree", &checkpoint_ref],
        )?;
        run_git(
            &self.git_dir,
            &self.work_tree,
            ["checkout-index", "-f", "-a"],
        )?;
        for file in created {
            let _ = std::fs::remove_file(self.work_tree.join(file));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn store_for(data_root: &Path, project: &Path) -> CheckpointStore {
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.to_str().expect("utf8 temp path")),
        )]);
        CheckpointStore::for_project(project).expect("store")
    }

    #[test]
    fn snapshot_then_modify_then_restore_returns_the_tree() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        fs::write(proj.join("a.txt"), "v1").unwrap();
        let store = store_for(data_root.path(), proj);

        let (id, _tree) = store.snapshot("t1").unwrap();
        fs::write(proj.join("a.txt"), "v2").unwrap();
        fs::write(proj.join("b.txt"), "new").unwrap();

        store.restore(&id).unwrap();

        assert_eq!(
            fs::read_to_string(proj.join("a.txt")).unwrap(),
            "v1",
            "fichier suivi restauré"
        );
        assert!(
            !proj.join("b.txt").exists(),
            "fichier créé depuis le snapshot supprimé"
        );
    }

    /// ⛔ BARRIÈRE premortem PM5 — restore ne doit JAMAIS toucher les fichiers
    /// non-suivis étrangers au snapshot (pas de `git clean`). Ce test échoue si
    /// un futur dev remplace le reverse-diff par `git clean -fdx`.
    ///
    /// `secret.env` est gitignoré : c'est la protection réelle de
    /// `files_created_since` (`add -A` respecte nativement le `.gitignore`
    /// du work-tree, décorrélé du `--git-dir` du store — vérifié
    /// empiriquement). Sans `.gitignore`, un fichier étranger créé dans la
    /// fenêtre snapshot→restore est indiscernable d'un fichier créé par le
    /// tour lui-même (aucune des deux informations n'existe dans l'arbre
    /// git) — limite connue, documentée sur `restore`.
    ///
    /// `pristine.txt` (Fix A6a) renforce la barrière contre un mutant plus
    /// insidieux qu'un `git clean -fdx` pur : un remplacement du
    /// reverse-diff par une suppression "tout ce qui n'est pas de ce tour"
    /// plutôt que "tout ce qui est absent de l'index post-restore" — un
    /// fichier non-gitignoré déjà présent avant le snapshot (donc capturé
    /// dans l'arbre cible T1 par le premier `add -A`) doit survivre au même
    /// titre que le secret gitignoré.
    #[test]
    fn restore_preserves_untracked_files_outside_the_snapshot() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        fs::write(proj.join(".gitignore"), "secret.env\n").unwrap();
        fs::write(proj.join("a.txt"), "v1").unwrap();
        fs::write(proj.join("pristine.txt"), "already here").unwrap();
        let store = store_for(data_root.path(), proj);

        let (id, _) = store.snapshot("t1").unwrap();
        fs::write(proj.join("secret.env"), "TOKEN=xyz").unwrap(); // gitignoré, jamais snapshoté
        fs::write(proj.join("a.txt"), "v2").unwrap();

        store.restore(&id).unwrap();

        assert_eq!(fs::read_to_string(proj.join("a.txt")).unwrap(), "v1");
        assert!(
            proj.join("secret.env").exists(),
            "un non-suivi étranger (gitignoré) doit survivre au restore"
        );
        assert!(
            proj.join("pristine.txt").exists(),
            "un fichier non-gitignoré déjà présent avant le snapshot doit aussi survivre"
        );
    }

    #[test]
    fn files_created_since_lists_only_additions() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        fs::write(proj.join("a.txt"), "v1").unwrap();
        let store = store_for(data_root.path(), proj);

        let (_, tree) = store.snapshot("t1").unwrap();
        fs::write(proj.join("a.txt"), "v2").unwrap(); // modif, pas ajout
        fs::write(proj.join("c.txt"), "new").unwrap(); // ajout

        let created = store.files_created_since(&tree).unwrap();

        assert_eq!(created, vec![std::path::PathBuf::from("c.txt")]);
    }

    /// BARRIÈRE — un refactor qui renomme un fichier est le cas le plus banal
    /// d'un tour d'agent. `git diff --diff-filter=A` avec la détection de
    /// rename (`diff.renames=true` par défaut depuis git 2.9) rend
    /// `a.txt → b.txt` comme un `R100` et n'émet AUCUNE ligne `A` : mesuré
    /// vide sur git 2.50.1. `b.txt` n'entrait donc pas dans `created`, le
    /// restore recréait `a.txt` sans supprimer `b.txt`, et annonçait
    /// « arbre aligné » sur un arbre qui contenait un doublon — avec un
    /// comportement dépendant de la config git du poste.
    #[test]
    fn files_created_since_sees_a_renamed_file_as_created() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        fs::write(proj.join("a.txt"), "v1").unwrap();
        let store = store_for(data_root.path(), proj);

        let (id, tree) = store.snapshot("t1").unwrap();
        fs::rename(proj.join("a.txt"), proj.join("b.txt")).unwrap();

        let created = store.files_created_since(&tree).unwrap();
        assert_eq!(
            created,
            vec![std::path::PathBuf::from("b.txt")],
            "la cible d'un renommage est un fichier créé depuis le checkpoint"
        );

        store.restore(&id).unwrap();
        assert_eq!(
            fs::read_to_string(proj.join("a.txt")).unwrap(),
            "v1",
            "le fichier d'origine est restauré"
        );
        assert!(
            !proj.join("b.txt").exists(),
            "le restore doit supprimer la cible du renommage, pas laisser un doublon"
        );
    }

    /// BARRIÈRE — un dépôt git imbriqué non-gitignoré (submodule, clone
    /// vendored) est enregistré par `add -A` comme gitlink (`160000 commit`),
    /// que `ls-tree -r --name-only` rend comme un chemin PLAT. Sur disque
    /// c'est un dossier : le cas 1 de `refuse_on_destructive_type_conflict`
    /// walkait donc son contenu, n'en trouvait aucun dans `created` (le diff
    /// ne voit que l'entrée gitlink) et refusait TOUS les restores du projet,
    /// définitivement, en accusant le submodule. `checkout-index` ne
    /// matérialise de toute façon jamais un gitlink : ces entrées n'ont rien
    /// à faire dans la liste des chemins que le restore va écrire.
    #[test]
    fn restore_ignores_nested_git_repository_gitlinks() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        fs::write(proj.join("a.txt"), "v1").unwrap();

        let nested = proj.join("vendor/lib");
        fs::create_dir_all(&nested).unwrap();
        for args in [
            vec!["init", "-q", nested.to_str().unwrap()],
            vec![
                "-C",
                nested.to_str().unwrap(),
                "config",
                "user.email",
                "t@e.st",
            ],
            vec!["-C", nested.to_str().unwrap(), "config", "user.name", "t"],
        ] {
            assert!(git_command().args(&args).status().unwrap().success());
        }
        fs::write(nested.join("lib.txt"), "libv1").unwrap();
        for args in [
            vec!["-C", nested.to_str().unwrap(), "add", "-A"],
            vec!["-C", nested.to_str().unwrap(), "commit", "-qm", "init"],
        ] {
            assert!(git_command().args(&args).status().unwrap().success());
        }

        let store = store_for(data_root.path(), proj);
        let (id, _tree) = store.snapshot("t1").unwrap();
        fs::write(proj.join("a.txt"), "v2").unwrap();

        store
            .restore(&id)
            .expect("un dépôt imbriqué ne doit pas rendre le restore impossible");

        assert_eq!(
            fs::read_to_string(proj.join("a.txt")).unwrap(),
            "v1",
            "le reste de l'arbre est bien restauré"
        );
        assert_eq!(
            fs::read_to_string(nested.join("lib.txt")).unwrap(),
            "libv1",
            "le contenu du dépôt imbriqué est laissé intact (jamais capturé, jamais écrasé)"
        );
    }

    /// Fix A4 — RED avant `-z`: `git diff --name-only` (sans `-z`) octal-
    /// échappe et quote tout octet non-ASCII (`core.quotePath` par défaut),
    /// donnant `"caf\303\251.txt"` au lieu de `café.txt`. `restore`
    /// utilisait ce chemin littéralement pour `remove_file`, échouait
    /// silencieusement (ENOENT avalé par `let _ = ...`), et le fichier
    /// survivait au restore contrairement au contrat.
    #[test]
    fn files_created_since_handles_non_ascii_filenames() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        fs::write(proj.join("a.txt"), "v1").unwrap();
        let store = store_for(data_root.path(), proj);

        let (id, tree) = store.snapshot("t1").unwrap();
        fs::write(proj.join("café.txt"), "non-ascii").unwrap();

        let created = store.files_created_since(&tree).unwrap();
        assert_eq!(
            created,
            vec![std::path::PathBuf::from("café.txt")],
            "non-ASCII filenames must come back decoded, not quoted/escaped"
        );

        store.restore(&id).unwrap();
        assert!(
            !proj.join("café.txt").exists(),
            "restore must actually delete the non-ASCII file it created since the checkpoint"
        );
    }

    /// Fix A1 — RED avant la détection de conflit de type : un refactor
    /// ordinaire (`rm config; mkdir config; ...`) transforme un fichier
    /// checkpointé en dossier. `checkout-index -f -a` doit alors supprimer
    /// récursivement le dossier disque pour écrire le blob `config` — y
    /// compris `config/.env.local`, gitignoré, jamais vu par
    /// `files_created_since`. `restore` doit refuser plutôt que perdre ce
    /// secret silencieusement.
    #[test]
    fn restore_refuses_file_to_directory_type_conflict_with_untracked_content() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        fs::write(proj.join("config"), "v1").unwrap();
        let store = store_for(data_root.path(), proj);

        let (id, _tree) = store.snapshot("t1").unwrap();

        fs::remove_file(proj.join("config")).unwrap();
        fs::create_dir(proj.join("config")).unwrap();
        fs::write(proj.join("config").join("app.rs"), "fn main() {}").unwrap();
        fs::write(proj.join(".gitignore"), "config/.env.local\n").unwrap();
        fs::write(proj.join("config").join(".env.local"), "SECRET=xyz").unwrap();

        let result = store.restore(&id);

        assert!(
            result.is_err(),
            "restore must refuse a file->directory type conflict with untracked content in the way"
        );
        assert!(
            proj.join("config").join(".env.local").exists(),
            "the gitignored secret must survive the refused restore"
        );
        assert!(
            proj.join("config").join("app.rs").exists(),
            "the refusal must be a true no-op — nothing gets touched"
        );
    }

    /// Fix A2 — RED avant l'alignement work-tree/clé : `project_key` hashait
    /// le toplevel du repo mais les opérations git utilisaient le chemin brut
    /// passé par l'appelant. Un `kaji` lancé depuis un sous-dossier ne
    /// snapshotait donc que ce sous-dossier, alors que deux sous-dossiers du
    /// même repo partagent le même store (même clé) — collision. Ici,
    /// `for_project` reçoit délibérément le sous-dossier `sub/`, et on
    /// vérifie que le snapshot a bien capturé tout le repo (racine incluse).
    #[test]
    fn snapshot_and_restore_use_the_git_toplevel_not_the_launch_subdir() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let repo = root.path();
        let sub = repo.join("sub");
        fs::create_dir(&sub).unwrap();
        let init = git_command()
            .arg("-C")
            .arg(repo)
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(init.status.success(), "git init must succeed");
        fs::write(repo.join("root.txt"), "root-v1").unwrap();
        fs::write(sub.join("child.txt"), "child-v1").unwrap();

        let store = store_for(data_root.path(), &sub); // for_project reçoit le SOUS-DOSSIER

        let (id, _tree) = store.snapshot("t1").unwrap();

        fs::write(repo.join("root.txt"), "root-v2").unwrap();
        fs::write(sub.join("child.txt"), "child-v2").unwrap();

        store.restore(&id).unwrap();

        assert_eq!(
            fs::read_to_string(repo.join("root.txt")).unwrap(),
            "root-v1",
            "le fichier à la racine du repo doit être capturé par le snapshot, \
             même si for_project a reçu le sous-dossier"
        );
        assert_eq!(
            fs::read_to_string(sub.join("child.txt")).unwrap(),
            "child-v1"
        );
    }

    /// Fix A5 — RED avant validation explicite : `--upload-pack=x` et `; rm`
    /// finissaient par échouer via `git rev-parse` (revision ambiguë), mais
    /// pour la mauvaise raison — le code ne validait rien, il dépendait
    /// implicitement du parsing de git. `validate_checkpoint_id` doit
    /// refuser AVANT tout appel git, avec un message explicite.
    #[test]
    fn restore_rejects_a_malformed_checkpoint_id() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        fs::write(proj.join("a.txt"), "v1").unwrap();
        let store = store_for(data_root.path(), proj);
        let _ = store.snapshot("t1").unwrap();

        for bad_id in ["--upload-pack=x", "; rm -rf /", "", "not-hex-zzzz"] {
            let result = store.restore(&CheckpointId(bad_id.to_string()));
            let error = result.expect_err(&format!("id {bad_id:?} must be rejected"));
            assert!(
                error.to_string().contains("invalide"),
                "expected explicit id validation error for {bad_id:?}, got: {error}"
            );
        }
    }

    /// ⛔ BARRIÈRE premortem PM7 — la fonction project_key est un contrat de
    /// compat : la changer orphelinerait tous les stores existants. Ce test
    /// fige la sortie pour une entrée connue.
    ///
    /// Fix A6b: `project_key` est désormais pure (aucun accès filesystem —
    /// voir sa doc), donc ce test ne dépend plus de la résolution de `/tmp`
    /// (symlink vers `/private/tmp` sur macOS) ni de l'existence du chemin.
    #[test]
    fn project_key_is_stable_for_a_known_path() {
        let k = project_key(Path::new("/tmp/kaji-fixture-proj"));
        assert_eq!(k.len(), 16, "sha256 tronqué 16 hex");
        assert_eq!(
            k, "cf9dea7f52710123",
            "project_key ne doit jamais changer sans migration"
        );
    }

    /// ⛔ BARRIÈRE premortem PM6 (finding D2) — le `Mutex` d'instance ne
    /// sérialise que l'intra-processus : deux sessions kaji sur le même
    /// projet partagent le même `git_dir` et se battent sur `index.lock`
    /// sans qu'aucun verrou ne les arrête. La sérialisation inter-processus
    /// est un flock exclusif NON-bloquant sur `<git_dir>/kaji-store.lock` :
    /// quand une autre instance le tient, l'opération est REFUSÉE
    /// explicitement (principe « refuser plutôt que perdre des données »),
    /// jamais mise en attente ni intercalée.
    #[test]
    fn snapshot_refuses_when_another_process_holds_the_store_lock() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        )]);
        fs::write(proj.join("a.txt"), "v1").unwrap();
        let store = CheckpointStore::for_project(proj).expect("store");

        let git_dir = Paths::in_data_dir("kaji/checkpoints")
            .join(format!("{}.git", project_key(&resolve_work_tree(proj))));
        let lock_path = git_dir.join("kaji-store.lock");
        std::fs::create_dir_all(&git_dir).unwrap();
        let holder = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .unwrap();
        holder.lock_exclusive().unwrap();

        let result = store.snapshot("t1");
        let error =
            result.expect_err("un store tenu par un autre processus doit refuser le snapshot");
        assert!(
            error.to_string().contains("occupé"),
            "le refus doit être explicite, pas un deadlock git: {error}"
        );
    }

    /// Miroir du test ci-dessus sur le restore : refuser AVANT toute mutation
    /// de l'arbre est le comportement attendu quand un autre processus tient
    /// le store.
    #[test]
    fn restore_refuses_when_another_process_holds_the_store_lock() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        )]);
        fs::write(proj.join("a.txt"), "v1").unwrap();
        let store = CheckpointStore::for_project(proj).expect("store");
        let (id, _) = store.snapshot("t1").unwrap();

        let git_dir = Paths::in_data_dir("kaji/checkpoints")
            .join(format!("{}.git", project_key(&resolve_work_tree(proj))));
        let holder = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(git_dir.join("kaji-store.lock"))
            .unwrap();
        holder.lock_exclusive().unwrap();

        fs::write(proj.join("a.txt"), "v2").unwrap();
        let result = store.restore(&id);
        let error =
            result.expect_err("un store tenu par un autre processus doit refuser le restore");
        assert!(
            error.to_string().contains("occupé"),
            "le refus doit nommer la cause: {error}"
        );
        assert_eq!(
            fs::read_to_string(proj.join("a.txt")).unwrap(),
            "v2",
            "le restore refusé ne doit rien avoir muté"
        );
    }

    /// When the lock is free, a second acquisition in the *same* process
    /// still works (flock is advisory between independent open file
    /// descriptions) — guards against the flock ever becoming sticky after a
    /// normal op.
    #[test]
    fn store_ops_succeed_when_the_lock_is_free() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        let _guard = env_lock::lock_env([(
            "KAJI_PATH_ROOT",
            Some(data_root.path().to_str().expect("utf8 temp path")),
        )]);
        fs::write(proj.join("a.txt"), "v1").unwrap();
        let store = CheckpointStore::for_project(proj).expect("store");

        let (id, _) = store.snapshot("t1").expect("snapshot sans contention");
        fs::write(proj.join("a.txt"), "v2").unwrap();
        store.restore(&id).expect("restore sans contention");
        assert_eq!(fs::read_to_string(proj.join("a.txt")).unwrap(), "v1");
    }

    fn git_init(dir: &Path) {
        let init = git_command()
            .arg("-C")
            .arg(dir)
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(init.status.success(), "git init must succeed");
    }

    #[test]
    fn eligibility_accepts_a_git_repo_and_its_subdirectories() {
        let root = TempDir::new().unwrap();
        let repo = root.path();
        git_init(repo);
        let sub = repo.join("sub");
        fs::create_dir(&sub).unwrap();

        assert_eq!(eligibility(repo, None), Ok(()));
        assert_eq!(eligibility(&sub, None), Ok(()));
    }

    #[test]
    fn eligibility_refuses_a_directory_outside_any_git_repo() {
        let root = TempDir::new().unwrap();

        assert_eq!(
            eligibility(root.path(), None),
            Err(CheckpointIneligible::NotAGitWorkTree)
        );
    }

    /// Un home versionné (dotfiles) est un dépôt git valide : c'est bien le
    /// coût du `add -A` sur tout le home qu'on refuse, pas l'absence de repo.
    #[test]
    fn eligibility_refuses_the_home_directory_even_when_it_is_a_repo() {
        let home = TempDir::new().unwrap();
        git_init(home.path());

        assert_eq!(
            eligibility(home.path(), Some(home.path())),
            Err(CheckpointIneligible::HomeDirectory)
        );
    }

    /// Le toplevel compte autant que `dir` : lancé depuis `~/sous-dossier`
    /// d'un home versionné, le snapshot couvrirait quand même tout le home.
    #[test]
    fn eligibility_refuses_a_subdirectory_of_a_versioned_home() {
        let home = TempDir::new().unwrap();
        git_init(home.path());
        let sub = home.path().join("workspace");
        fs::create_dir(&sub).unwrap();

        assert_eq!(
            eligibility(&sub, Some(home.path())),
            Err(CheckpointIneligible::HomeDirectory)
        );
    }

    #[test]
    fn eligibility_refuses_the_filesystem_root() {
        assert_eq!(
            eligibility(Path::new("/"), None),
            Err(CheckpointIneligible::FilesystemRoot)
        );
    }
}
