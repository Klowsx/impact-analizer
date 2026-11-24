use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{FaultlineError, Result};

/// Read-only view into a local Git repository.
///
/// All operations read directly from Git's object database (`rev-parse`, `show`,
/// `ls-tree`) without modifying the user's working tree or running checkout.
#[derive(Debug, Clone)]
pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves any Git ref (branch, tag, HEAD, short SHA) into a full 40-char commit SHA.
    pub fn resolve_revision(&self, rev: &str) -> Result<String> {
        let target = format!("{rev}^{{commit}}");
        let sha = self.run_git(&["rev-parse", "--verify", &target])?;
        Ok(sha.trim().to_string())
    }

    /// Reads the UTF-8 contents of a file at a given commit without checking it out.
    pub fn read_file(&self, rev: &str, relative_path: &Path) -> Result<String> {
        let posix_path = to_git_posix_path(relative_path);
        let spec = format!("{rev}:{posix_path}");
        self.run_git(&["show", &spec])
    }

    /// Lists all tracked file paths (POSIX-style) in the repository at a given commit.
    pub fn list_files(&self, rev: &str) -> Result<Vec<String>> {
        let output = self.run_git(&["ls-tree", "-r", "--name-only", rev])?;
        let mut files: Vec<String> = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        files.sort();
        Ok(files)
    }

    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .output()
            .map_err(|source| FaultlineError::Io {
                path: self.root.clone(),
                source,
            })?;

        if !output.status.success() {
            return Err(FaultlineError::Git {
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
        let tmp = std::env::temp_dir().join(format!("faultline-git-test-{}", std::process::id()));
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

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rejects_nonexistent_revision() {
        let tmp =
            std::env::temp_dir().join(format!("faultline-git-err-test-{}", std::process::id()));
        let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");

        let repo = GitRepo::new(&paths.customer_service);
        let err = repo
            .resolve_revision("branch-that-does-not-exist")
            .unwrap_err();
        assert!(matches!(err, FaultlineError::Git { .. }));

        let _ = fs::remove_dir_all(&tmp);
    }
}
