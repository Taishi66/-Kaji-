use crate::config::paths::Paths;
use crate::subprocess::git_command;
use crate::utils::bytes_to_hex;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Short id of a checkpoint commit (`refs/kaji/<id>` in the store). Not a
/// full sha — collisions are theoretically possible but astronomically
/// unlikely within a single project's checkpoint history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointId(pub String);

/// Git-bare-backed store of working-tree snapshots for one project.
///
/// Every operation goes through `--git-dir=<store>` with the caller's real
/// project directory as `--work-tree`, so the store never touches the
/// user's own `.git` (if any).
pub struct CheckpointStore {
    git_dir: PathBuf,
    /// premortem PM6: a bare repo has a single on-disk index. Concurrent
    /// `git` invocations against the same `--git-dir` (snapshot racing a
    /// restore, or two snapshots) fight over `index.lock` and can corrupt
    /// it. Serialization lives HERE, in the store, rather than being
    /// inherited from whatever single-threaded loop happens to call it
    /// today — a future concurrent caller (gateway/ACP) must not be able
    /// to bypass it.
    lock: Mutex<()>,
}

/// Identity key for a project's on-disk checkpoint store (`<key>.git` under
/// `kaji/checkpoints/`). FIGÉE (premortem PM7): changing the hashing or the
/// path-resolution logic below changes every project's key, orphaning all
/// existing stores silently (no error — they just look empty). Never modify
/// this function without a migration path for users' existing stores.
fn project_key(project: &Path) -> String {
    let base = git_toplevel(project).unwrap_or_else(|| project.to_path_buf());
    let canon = std::fs::canonicalize(&base).unwrap_or(base);
    let digest = Sha256::digest(canon.to_string_lossy().as_bytes());
    bytes_to_hex(digest).chars().take(16).collect()
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

impl CheckpointStore {
    pub fn for_project(project: &Path) -> Result<Self> {
        let git_dir =
            Paths::in_data_dir("kaji/checkpoints").join(format!("{}.git", project_key(project)));
        if !git_dir.exists() {
            std::fs::create_dir_all(&git_dir)
                .with_context(|| format!("creating {}", git_dir.display()))?;
            init_bare(&git_dir)?;
        }
        Ok(Self {
            git_dir,
            lock: Mutex::new(()),
        })
    }

    /// Snapshots the working tree and returns `(checkpoint id, tree sha)`.
    /// Chains onto `refs/kaji/latest` as parent when one exists, so the
    /// store's commit history is a linear timeline of snapshots.
    pub fn snapshot(&self, project: &Path, label: &str) -> Result<(CheckpointId, String)> {
        let _guard = self.lock.lock().unwrap();
        run_git(&self.git_dir, project, ["add", "-A"])?;
        let tree = run_git(&self.git_dir, project, ["write-tree"])?
            .trim()
            .to_string();
        let parent = run_git(
            &self.git_dir,
            project,
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
        let commit = run_git(&self.git_dir, project, &args)?.trim().to_string();
        let id: String = commit.chars().take(12).collect();
        run_git(
            &self.git_dir,
            project,
            ["update-ref", &format!("refs/kaji/{id}"), &commit],
        )?;
        run_git(
            &self.git_dir,
            project,
            ["update-ref", "refs/kaji/latest", &commit],
        )?;
        Ok((CheckpointId(id), tree))
    }

    /// Lists paths present in the current working tree but absent from
    /// `target_tree` — i.e. files created since that snapshot. Named and
    /// tested in isolation on purpose (premortem PM5): `restore` depends on
    /// it to know exactly which files it is allowed to delete, so its
    /// removal must be a visible, deliberate act, not an inlined detail.
    pub fn files_created_since(&self, project: &Path, target_tree: &str) -> Result<Vec<PathBuf>> {
        let _guard = self.lock.lock().unwrap();
        run_git(&self.git_dir, project, ["add", "-A"])?;
        let current = run_git(&self.git_dir, project, ["write-tree"])?
            .trim()
            .to_string();
        let out = run_git(
            &self.git_dir,
            project,
            [
                "diff",
                "--name-only",
                "--diff-filter=A",
                target_tree,
                &current,
            ],
        )?;
        Ok(out.lines().map(PathBuf::from).collect())
    }

    fn tree_of(&self, project: &Path, id: &CheckpointId) -> Result<String> {
        let _guard = self.lock.lock().unwrap();
        let checkpoint_ref = format!("refs/kaji/{}", id.0);
        let out = run_git(
            &self.git_dir,
            project,
            ["rev-parse", &format!("{checkpoint_ref}^{{tree}}")],
        )?;
        Ok(out.trim().to_string())
    }

    /// Restores the working tree to the state captured by `target`.
    ///
    /// premortem PM5: this NEVER runs `git clean`. `--work-tree` is the
    /// user's real project directory, which routinely holds untracked
    /// files no kaji snapshot ever captured (`.env`, `node_modules`, scratch
    /// files) — `git clean -fd` would delete all of them indiscriminately.
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
    pub fn restore(&self, project: &Path, target: &CheckpointId) -> Result<()> {
        let tree = self.tree_of(project, target)?;
        let created = self.files_created_since(project, &tree)?;

        let _guard = self.lock.lock().unwrap();
        let checkpoint_ref = format!("refs/kaji/{}", target.0);
        run_git(&self.git_dir, project, ["read-tree", &checkpoint_ref])?;
        run_git(&self.git_dir, project, ["checkout-index", "-f", "-a"])?;
        for file in created {
            let _ = std::fs::remove_file(project.join(file));
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

        let (id, _tree) = store.snapshot(proj, "t1").unwrap();
        fs::write(proj.join("a.txt"), "v2").unwrap();
        fs::write(proj.join("b.txt"), "new").unwrap();

        store.restore(proj, &id).unwrap();

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
    #[test]
    fn restore_preserves_untracked_files_outside_the_snapshot() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        fs::write(proj.join(".gitignore"), "secret.env\n").unwrap();
        fs::write(proj.join("a.txt"), "v1").unwrap();
        let store = store_for(data_root.path(), proj);

        let (id, _) = store.snapshot(proj, "t1").unwrap();
        fs::write(proj.join("secret.env"), "TOKEN=xyz").unwrap(); // gitignoré, jamais snapshoté
        fs::write(proj.join("a.txt"), "v2").unwrap();

        store.restore(proj, &id).unwrap();

        assert_eq!(fs::read_to_string(proj.join("a.txt")).unwrap(), "v1");
        assert!(
            proj.join("secret.env").exists(),
            "un non-suivi étranger (gitignoré) doit survivre au restore"
        );
    }

    #[test]
    fn files_created_since_lists_only_additions() {
        let data_root = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let proj = root.path();
        fs::write(proj.join("a.txt"), "v1").unwrap();
        let store = store_for(data_root.path(), proj);

        let (_, tree) = store.snapshot(proj, "t1").unwrap();
        fs::write(proj.join("a.txt"), "v2").unwrap(); // modif, pas ajout
        fs::write(proj.join("c.txt"), "new").unwrap(); // ajout

        let created = store.files_created_since(proj, &tree).unwrap();

        assert_eq!(created, vec![std::path::PathBuf::from("c.txt")]);
    }

    /// ⛔ BARRIÈRE premortem PM7 — la fonction project_key est un contrat de
    /// compat : la changer orphelinerait tous les stores existants. Ce test fige
    /// la sortie pour un chemin connu.
    #[test]
    fn project_key_is_stable_for_a_known_path() {
        let k = project_key(Path::new("/tmp/kaji-fixture-proj"));
        assert_eq!(k.len(), 16, "sha256 tronqué 16 hex");
        assert_eq!(
            k, "cf9dea7f52710123",
            "project_key ne doit jamais changer sans migration"
        );
    }
}
