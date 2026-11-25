use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{LaineError, Result};

/// Pseudo-revision representing uncommitted files on disk (working tree).
pub const WORKTREE_REV: &str = "WORKTREE";

/// Pseudo-revision representing files staged in Git's index.
pub const STAGED_REV: &str = "STAGED";

/// Returns true if `rev` refers to the local working tree pseudo-revision.
pub fn is_worktree_rev(rev: &str) -> bool {
    rev.eq_ignore_ascii_case(WORKTREE_REV)
}

/// Returns true if `rev` refers to Git's staged index pseudo-revision.
pub fn is_staged_rev(rev: &str) -> bool {
    rev.eq_ignore_ascii_case(STAGED_REV)
}

/// Read-only view into a local Git repository.
///
/// All operations read directly from Git's object database (`rev-parse`, `show`,
/// `ls-tree`, `cat-file --batch`), or from the local working tree / index when
/// `WORKTREE` or `STAGED` is requested, without modifying the user's working tree
/// or running checkout.
#[derive(Debug, Clone)]
pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    // Resuelve la raíz de un checkout Git desde cualquier directorio de su árbol.
    pub fn discover(path: &Path) -> Result<Self> {
        let git = Self::new(path);
        let root = git.run_git(&["rev-parse", "--show-toplevel"])?;
        Ok(Self::new(root.strip_suffix('\n').unwrap_or(&root)))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves any Git ref (branch, tag, HEAD, short SHA) into a full 40-char commit SHA,
    /// or validates and normalizes the `WORKTREE` / `STAGED` pseudo-revisions.
    pub fn resolve_revision(&self, rev: &str) -> Result<String> {
        if is_worktree_rev(rev) {
            self.run_git(&["rev-parse", "--git-dir"])?;
            return Ok(WORKTREE_REV.to_string());
        }
        if is_staged_rev(rev) {
            self.run_git(&["rev-parse", "--git-dir"])?;
            return Ok(STAGED_REV.to_string());
        }
        let target = format!("{rev}^{{commit}}");
        let sha = self.run_git(&["rev-parse", "--verify", &target])?;
        Ok(sha.trim().to_string())
    }

    /// Reads the UTF-8 contents of a single file at a given commit, staged index,
    /// or local working tree without checking anything out.
    pub fn read_file(&self, rev: &str, relative_path: &Path) -> Result<String> {
        if is_worktree_rev(rev) {
            let full_path = self.root.join(relative_path);
            let bytes = fs::read(&full_path).map_err(|source| LaineError::Io {
                path: full_path,
                source,
            })?;
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        let posix_path = to_git_posix_path(relative_path);
        let spec = if is_staged_rev(rev) {
            format!(":{posix_path}")
        } else {
            format!("{rev}:{posix_path}")
        };
        self.run_git(&["show", &spec])
    }

    /// Reads many files at a given commit, staged index, or local working tree.
    ///
    /// For committed and staged revisions, uses a single `git cat-file --batch` process
    /// to avoid per-file process spawn overhead. Missing or non-blob paths are skipped cleanly.
    pub fn read_files_batch(
        &self,
        rev: &str,
        relative_paths: &[String],
    ) -> Result<BTreeMap<String, String>> {
        if relative_paths.is_empty() {
            return Ok(BTreeMap::new());
        }

        if is_worktree_rev(rev) {
            let mut results = BTreeMap::new();
            for path in relative_paths {
                let full_path = self.root.join(path);
                if full_path.is_file()
                    && let Ok(bytes) = fs::read(&full_path)
                {
                    results.insert(path.clone(), String::from_utf8_lossy(&bytes).into_owned());
                }
            }
            return Ok(results);
        }

        let mut child = Command::new("git")
            .current_dir(&self.root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| LaineError::Io {
                path: self.root.clone(),
                source,
            })?;

        let mut stdin = child.stdin.take().ok_or_else(|| LaineError::Git {
            command: "git cat-file --batch".into(),
            message: "failed to open stdin pipe".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| LaineError::Git {
            command: "git cat-file --batch".into(),
            message: "failed to open stdout pipe".into(),
        })?;

        let staged = is_staged_rev(rev);
        let specs: Vec<String> = relative_paths
            .iter()
            .map(|p| {
                let posix = to_git_posix_path(Path::new(p));
                if staged {
                    format!(":{posix}")
                } else {
                    format!("{rev}:{posix}")
                }
            })
            .collect();

        // Write specs on a scoped thread so large batches never deadlock the pipe buffer.
        let mut results = BTreeMap::new();
        let read_result: Result<()> = std::thread::scope(|scope| {
            let writer = scope.spawn(move || {
                for spec in &specs {
                    let _ = writeln!(stdin, "{spec}");
                }
                let _ = stdin.flush();
                drop(stdin);
            });

            let mut reader = BufReader::new(stdout);
            let mut header = String::new();

            for path in relative_paths {
                header.clear();
                let n = reader
                    .read_line(&mut header)
                    .map_err(|source| LaineError::Io {
                        path: self.root.clone(),
                        source,
                    })?;
                if n == 0 {
                    break;
                }

                let trimmed = header.trim_end_matches(['\r', '\n']);
                if trimmed.ends_with(" missing") {
                    continue;
                }

                let mut parts = trimmed.split_whitespace();
                let _oid = parts.next();
                let obj_type = parts.next();
                let size_str = parts.next();

                let Some(size) = size_str.and_then(|s| s.parse::<usize>().ok()) else {
                    continue;
                };

                let mut buf = vec![0u8; size];
                reader
                    .read_exact(&mut buf)
                    .map_err(|source| LaineError::Io {
                        path: self.root.clone(),
                        source,
                    })?;

                // Consume the single trailing LF written by `git cat-file --batch`.
                let mut trailing = [0u8; 1];
                let _ = reader.read_exact(&mut trailing);

                if obj_type == Some("blob") {
                    results.insert(path.clone(), String::from_utf8_lossy(&buf).into_owned());
                }
            }

            let _ = writer.join();
            Ok(())
        });

        let output = child.wait_with_output().map_err(|source| LaineError::Io {
            path: self.root.clone(),
            source,
        })?;
        read_result?;

        if !output.status.success() {
            return Err(LaineError::Git {
                command: "git cat-file --batch".into(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        Ok(results)
    }

    /// Lists file paths (POSIX-style) in the repository at a given commit, staged index,
    /// or working tree (tracked + non-ignored untracked files existing on disk).
    pub fn list_files(&self, rev: &str) -> Result<Vec<String>> {
        let output = if is_worktree_rev(rev) {
            self.run_git(&["ls-files", "--cached", "--others", "--exclude-standard"])?
        } else if is_staged_rev(rev) {
            self.run_git(&["ls-files", "--cached"])?
        } else {
            self.run_git(&["ls-tree", "-r", "--name-only", rev])?
        };
        let worktree = is_worktree_rev(rev);
        let mut files: Vec<String> = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter(|line| !worktree || self.root.join(line).is_file())
            .map(ToOwned::to_owned)
            .collect();
        files.sort();
        files.dedup();
        Ok(files)
    }

    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .output()
            .map_err(|source| LaineError::Io {
                path: self.root.clone(),
                source,
            })?;

        if !output.status.success() {
            return Err(LaineError::Git {
                command: format!("git {}", args.join(" ")),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn to_git_posix_path(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    raw.trim_start_matches("./").to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fixtures::create_demo_workspace;

    #[test]
    fn reads_base_and_head_snapshots_without_touching_working_tree() {
        let tmp = std::env::temp_dir().join(format!("laine-git-test-{}", std::process::id()));
        let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");

        let repo = GitRepo::new(&paths.customer_service);
        let base_sha = repo.resolve_revision("main").expect("main should resolve");
        let head_sha = repo
            .resolve_revision("feature/split-customer-name")
            .expect("feature branch should resolve");

        assert_eq!(base_sha.len(), 40);
        assert_eq!(head_sha.len(), 40);
        assert_ne!(base_sha, head_sha);

        let base_openapi = repo
            .read_file(&base_sha, Path::new("openapi.yaml"))
            .expect("base openapi should be readable");
        let head_openapi = repo
            .read_file(&head_sha, Path::new("openapi.yaml"))
            .expect("head openapi should be readable");

        assert!(base_openapi.contains("fullName"));
        assert!(!head_openapi.contains("fullName"));
        assert!(head_openapi.contains("firstName"));

        let files = repo
            .list_files(&base_sha)
            .expect("list_files should succeed");
        assert_eq!(files, vec!["openapi.yaml", "src/routes/customers.ts"]);

        // Batch read matches single-file reads and skips missing entries cleanly.
        let mut batch_req = files.clone();
        batch_req.push("does/not/exist.ts".to_string());
        let batch = repo
            .read_files_batch(&base_sha, &batch_req)
            .expect("batch read should succeed");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch.get("openapi.yaml"), Some(&base_openapi));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rejects_nonexistent_revision() {
        let tmp = std::env::temp_dir().join(format!("laine-git-err-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");

        let repo = GitRepo::new(&paths.customer_service);
        let err = repo
            .resolve_revision("branch-that-does-not-exist")
            .unwrap_err();
        assert!(matches!(err, LaineError::Git { .. }));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reads_uncommitted_worktree_changes_and_untracked_files() {
        let tmp =
            std::env::temp_dir().join(format!("laine-git-worktree-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");

        // Modify a tracked file on disk without staging or committing,
        // add an untracked file, and delete another tracked file.
        fs::write(
            paths.customer_service.join("openapi.yaml"),
            "openapi: 3.0.3\ninfo:\n  title: modified-in-worktree\npaths: {}\n",
        )
        .unwrap();
        fs::write(
            paths.customer_service.join("src/routes/new_route.ts"),
            "app.get('/health', () => {});\n",
        )
        .unwrap();
        fs::remove_file(paths.customer_service.join("src/routes/customers.ts")).unwrap();

        let repo = GitRepo::new(&paths.customer_service);
        let wt_rev = repo
            .resolve_revision("worktree")
            .expect("worktree pseudo-revision should resolve");
        assert_eq!(wt_rev, WORKTREE_REV);

        let wt_openapi = repo
            .read_file(&wt_rev, Path::new("openapi.yaml"))
            .expect("worktree openapi should be readable");
        assert!(wt_openapi.contains("modified-in-worktree"));

        let head_openapi = repo
            .read_file("HEAD", Path::new("openapi.yaml"))
            .expect("HEAD openapi should remain unchanged");
        assert!(!head_openapi.contains("modified-in-worktree"));

        let files = repo
            .list_files(&wt_rev)
            .expect("list_files on WORKTREE should succeed");
        assert!(files.contains(&"openapi.yaml".to_string()));
        assert!(files.contains(&"src/routes/new_route.ts".to_string()));
        assert!(!files.contains(&"src/routes/customers.ts".to_string()));

        let batch = repo
            .read_files_batch(
                &wt_rev,
                &[
                    "openapi.yaml".to_string(),
                    "src/routes/new_route.ts".to_string(),
                    "src/routes/customers.ts".to_string(),
                ],
            )
            .expect("batch read on WORKTREE should succeed");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch.get("openapi.yaml"), Some(&wt_openapi));
        assert!(batch.contains_key("src/routes/new_route.ts"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reads_staged_index_changes_separately_from_worktree() {
        let tmp =
            std::env::temp_dir().join(format!("laine-git-staged-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");

        // Stage one version of openapi.yaml, then overwrite disk with a different version.
        fs::write(
            paths.customer_service.join("openapi.yaml"),
            "openapi: 3.0.3\ninfo:\n  title: staged-version\npaths: {}\n",
        )
        .unwrap();
        let status = Command::new("git")
            .current_dir(&paths.customer_service)
            .args(["add", "openapi.yaml"])
            .status()
            .expect("git add should run");
        assert!(status.success());

        fs::write(
            paths.customer_service.join("openapi.yaml"),
            "openapi: 3.0.3\ninfo:\n  title: unstaged-disk-version\npaths: {}\n",
        )
        .unwrap();

        let repo = GitRepo::new(&paths.customer_service);
        let staged_rev = repo
            .resolve_revision("staged")
            .expect("staged pseudo-revision should resolve");
        assert_eq!(staged_rev, STAGED_REV);

        let staged_openapi = repo
            .read_file(&staged_rev, Path::new("openapi.yaml"))
            .expect("staged openapi should be readable");
        assert!(staged_openapi.contains("staged-version"));
        assert!(!staged_openapi.contains("unstaged-disk-version"));

        let batch = repo
            .read_files_batch(&staged_rev, &["openapi.yaml".to_string()])
            .expect("batch read on STAGED should succeed");
        assert_eq!(batch.get("openapi.yaml"), Some(&staged_openapi));

        let files = repo
            .list_files(&staged_rev)
            .expect("list_files on STAGED should succeed");
        assert!(files.contains(&"openapi.yaml".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }
}
