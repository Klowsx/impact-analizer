use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
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
        let root = git.run_git_utf8(&["rev-parse", "--show-toplevel"])?;
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
            self.run_git_utf8(&["rev-parse", "--git-dir"])?;
            return Ok(WORKTREE_REV.to_string());
        }
        if is_staged_rev(rev) {
            self.run_git_utf8(&["rev-parse", "--git-dir"])?;
            return Ok(STAGED_REV.to_string());
        }
        let target = format!("{rev}^{{commit}}");
        let sha = self.run_git_utf8(&["rev-parse", "--verify", &target])?;
        Ok(sha.trim().to_string())
    }

    /// Lee un archivo UTF-8 regular del commit, índice o árbol local, sin seguir enlaces.
    pub fn read_file(&self, rev: &str, relative_path: &Path) -> Result<String> {
        let posix_path = to_git_posix_path(relative_path);
        if is_worktree_rev(rev) {
            let links = self.git_link_paths(STAGED_REV, Some(&posix_path))?;
            return read_worktree_file(&self.canonical_worktree_root()?, &posix_path, &links);
        }
        if self
            .git_link_paths(rev, Some(&posix_path))?
            .contains(&posix_path)
        {
            return Err(link_not_supported(relative_path));
        }
        let spec = if is_staged_rev(rev) {
            format!(":{posix_path}")
        } else {
            format!("{rev}:{posix_path}")
        };
        let bytes = self.run_git_bytes(&["cat-file", "blob", &spec])?;
        decode_source(bytes, relative_path)
    }

    // Usa la raíz física como límite para las lecturas en disco.
    fn canonical_worktree_root(&self) -> Result<PathBuf> {
        self.root.canonicalize().map_err(|source| LaineError::Io {
            path: self.root.clone(),
            source,
        })
    }

    // Obtiene las rutas registradas en Git con modo de enlace simbólico.
    fn git_link_paths(&self, rev: &str, path: Option<&str>) -> Result<BTreeSet<String>> {
        let mut args = if is_staged_rev(rev) {
            vec!["--literal-pathspecs", "ls-files", "-z", "--stage"]
        } else {
            vec!["--literal-pathspecs", "ls-tree", "-rz", rev]
        };
        if let Some(path) = path {
            args.extend(["--", path]);
        }
        let output = self.run_git_utf8(&args)?;
        Ok(output
            .split_terminator('\0')
            .filter_map(|entry| {
                let (object, file) = entry.split_once('\t')?;
                object.starts_with("120000 ").then(|| file.to_owned())
            })
            .collect())
    }

    /// Reads files from a commit, the index, or the worktree, keeping their paths.
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
            let links = self.git_link_paths(STAGED_REV, None)?;
            let root = self.canonical_worktree_root()?;
            for path in relative_paths {
                if let Ok(raw) = read_worktree_file(&root, path, &links) {
                    results.insert(path.clone(), raw);
                }
            }
            return Ok(results);
        }

        // The batch protocol uses lines. Pass paths with line breaks as separate arguments.
        let links = self.git_link_paths(rev, None)?;
        let (line_paths, unusual_paths): (Vec<_>, Vec<_>) = relative_paths
            .iter()
            .filter(|path| links.is_empty() || !links.contains(&to_git_posix_path(Path::new(path))))
            .partition(|path| !path.contains(['\r', '\n']));
        let mut results = BTreeMap::new();
        for path in unusual_paths {
            if let Ok(raw) = self.read_file(rev, Path::new(path)) {
                results.insert(path.clone(), raw);
            }
        }
        if line_paths.is_empty() {
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

        let stdin = child.stdin.take().ok_or_else(|| LaineError::Git {
            command: "git cat-file --batch".into(),
            message: "failed to open stdin pipe".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| LaineError::Git {
            command: "git cat-file --batch".into(),
            message: "failed to open stdout pipe".into(),
        })?;

        let staged = is_staged_rev(rev);
        let prefix = if staged { "" } else { rev };

        // Write specs on a scoped thread so large batches never deadlock the pipe buffer.
        let read_result: Result<()> = std::thread::scope(|scope| {
            let write_paths = &line_paths;
            let writer = scope.spawn(move || {
                let mut stdin = BufWriter::new(stdin);
                for path in write_paths {
                    let posix = to_git_posix_path(Path::new(path));
                    if writeln!(stdin, "{prefix}:{posix}").is_err() {
                        break;
                    }
                }
                let _ = stdin.flush();
                drop(stdin);
            });

            let mut reader = BufReader::new(stdout);
            let mut header = String::new();

            for path in line_paths.iter().copied() {
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

                let Some(size) = size_str.and_then(|s| s.parse::<u64>().ok()) else {
                    continue;
                };

                let io_error = |source| LaineError::Io {
                    path: self.root.clone(),
                    source,
                };
                let mut object = reader.by_ref().take(size);
                // Reserve only a small buffer up front and grow it with bytes actually read.
                const MAX_INITIAL_BLOB_CAPACITY: u64 = 64 * 1024;
                let mut bytes = if obj_type == Some("blob") {
                    let mut bytes =
                        Vec::with_capacity(size.min(MAX_INITIAL_BLOB_CAPACITY) as usize);
                    object.read_to_end(&mut bytes).map_err(io_error)?;
                    bytes
                } else {
                    io::copy(&mut object, &mut io::sink()).map_err(io_error)?;
                    Vec::new()
                };
                if object.limit() != 0 {
                    return Err(io_error(io::Error::from(io::ErrorKind::UnexpectedEof)));
                }

                // Consume the single trailing LF written by `git cat-file --batch`.
                let mut trailing = [0u8; 1];
                let _ = reader.read_exact(&mut trailing);

                if obj_type == Some("blob") {
                    // Retain only the source text's capacity for the rest of the snapshot.
                    bytes.shrink_to_fit();
                    if let Ok(source) = decode_source(bytes, Path::new(path)) {
                        results.insert(path.clone(), source);
                    }
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

    /// Enumera archivos de un commit, índice o árbol local con rutas delimitadas por NUL.
    pub fn list_files(&self, rev: &str) -> Result<Vec<String>> {
        let output = if is_worktree_rev(rev) {
            self.run_git_utf8(&[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])?
        } else if is_staged_rev(rev) {
            self.run_git_utf8(&["ls-files", "-z", "--cached"])?
        } else {
            self.run_git_utf8(&["ls-tree", "-rz", "--name-only", rev])?
        };
        let worktree = is_worktree_rev(rev);
        let mut files: Vec<String> = output
            .split_terminator('\0')
            .filter(|line| !line.is_empty())
            .filter(|line| {
                !worktree
                    || match fs::symlink_metadata(self.root.join(line)) {
                        Ok(metadata) => metadata.is_file() || is_link(&metadata),
                        Err(error) => !matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ),
                    }
            })
            .map(ToOwned::to_owned)
            .collect();
        files.sort();
        files.dedup();
        Ok(files)
    }

    // Obtiene los renombres detectados por Git entre revisiones, incluyendo índice y árbol local.
    pub fn renamed_files(&self, base: &str, head: &str) -> Result<Vec<(String, String)>> {
        let staged = is_staged_rev;
        let worktree = is_worktree_rev;
        if base == head {
            return Ok(Vec::new());
        }
        let mut args = vec!["diff", "--name-status", "-z", "-M", "--no-ext-diff"];
        let reverse = if staged(base) && worktree(head) {
            false
        } else if worktree(base) && staged(head) {
            true
        } else if staged(base) || staged(head) {
            args.extend(["--cached", if staged(base) { head } else { base }]);
            staged(base)
        } else if worktree(base) || worktree(head) {
            args.push(if worktree(base) { head } else { base });
            worktree(base)
        } else {
            args.extend([base, head]);
            false
        };
        args.push("--");
        let raw = self.run_git_utf8(&args)?;
        let mut entries = raw.split_terminator('\0');
        let mut renamed = Vec::new();
        while let (Some(status), Some(path)) = (entries.next(), entries.next()) {
            if status.starts_with('R') || status.starts_with('C') {
                let Some(destination) = entries.next() else {
                    break;
                };
                if status.starts_with('R') {
                    let (before, after) = if reverse {
                        (destination, path)
                    } else {
                        (path, destination)
                    };
                    renamed.push((before.to_owned(), after.to_owned()));
                }
            }
        }
        Ok(renamed)
    }

    // Lee salidas de control de Git que deben contener texto UTF-8 válido.
    fn run_git_utf8(&self, args: &[&str]) -> Result<String> {
        decode_source(self.run_git_bytes(args)?, &self.root)
    }

    // Ejecuta Git sin alterar los bytes de la salida original.
    fn run_git_bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
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

        Ok(output.stdout)
    }
}

// Rechaza enlaces y rutas externas antes de abrir el archivo local como UTF-8.
fn read_worktree_file(
    root: &Path,
    relative_path: &str,
    indexed_links: &BTreeSet<String>,
) -> Result<String> {
    let normalized = to_git_posix_path(Path::new(relative_path));
    let relative = Path::new(&normalized);
    if indexed_links.contains(&normalized) {
        return Err(link_not_supported(relative));
    }
    let io_error = |source| LaineError::Io {
        path: relative.to_path_buf(),
        source,
    };
    let mut path = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(io_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a path inside the git checkout",
            )));
        };
        path.push(component);
        if is_link(&fs::symlink_metadata(&path).map_err(io_error)?) {
            return Err(link_not_supported(relative));
        }
    }
    let path = path.canonicalize().map_err(io_error)?;
    if !path.starts_with(root) {
        return Err(link_not_supported(relative));
    }
    if !fs::metadata(&path).map_err(io_error)?.is_file() {
        return Err(io_error(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        )));
    }
    decode_source(fs::read(path).map_err(io_error)?, relative)
}

// Detecta enlaces y puntos de reanálisis de Windows como junctions.
fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

// Nombra la ruta descartada para que el reporte señale evidencia incompleta.
fn link_not_supported(path: &Path) -> LaineError {
    LaineError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::PermissionDenied,
            "symbolic links are not read from snapshots",
        ),
    }
}

// Devuelve un error localizado en vez de sustituir bytes inválidos.
fn decode_source(bytes: Vec<u8>, path: &Path) -> Result<String> {
    String::from_utf8(bytes).map_err(|error| LaineError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, error.utf8_error()),
    })
}

// Convierte separadores de Windows y conserva una barra invertida literal en Unix.
fn to_git_posix_path(path: &Path) -> String {
    let raw = if cfg!(windows) {
        path.to_string_lossy().replace('\\', "/")
    } else {
        path.to_string_lossy().into_owned()
    };
    raw.trim_start_matches("./").to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fixtures::create_demo_workspace;

    // Check that streaming over a large blob and a tree keeps subsequent objects aligned.
    #[test]
    fn batch_reads_large_blobs_verbatim_and_skips_non_blobs() {
        let tmp = std::env::temp_dir().join(format!("laine-git-large-blob-{}", std::process::id()));
        let paths = create_demo_workspace(&tmp).unwrap();
        let repo = GitRepo::new(&paths.customer_service);
        let source = format!("\u{feff}{}", "data\0é\n".repeat(20_000));
        fs::write(repo.root().join("large.ts"), &source).unwrap();
        repo.run_git_utf8(&["add", "large.ts"]).unwrap();
        repo.run_git_utf8(&["commit", "-m", "test: add large blob"])
            .unwrap();
        let revision = repo.resolve_revision("HEAD").unwrap();

        let requested = ["large.ts", "src", "openapi.yaml"].map(str::to_owned);
        let batch = repo.read_files_batch(&revision, &requested).unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch["large.ts"], source);
        assert_eq!(
            batch["openapi.yaml"],
            repo.read_file(&revision, Path::new("openapi.yaml"))
                .unwrap()
        );
        fs::remove_dir_all(&tmp).unwrap();
    }

    // Verifica que commits, índice y disco rechacen bytes y entradas de enlace no analizables.
    #[test]
    fn rejects_non_utf8_and_indexed_links_in_every_snapshot() {
        let tmp = std::env::temp_dir().join(format!("laine-git-unreadable-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let paths = create_demo_workspace(&tmp).unwrap();
        let repo = GitRepo::new(&paths.customer_service);
        fs::write(repo.root().join("binary.ts"), [0x66, 0x80]).unwrap();
        fs::write(repo.root().join("link.ts"), "../../outside.ts").unwrap();
        let oid = repo
            .run_git_utf8(&["hash-object", "-w", "link.ts"])
            .unwrap();
        let link = format!("120000,{},link.ts", oid.trim_end());
        repo.run_git_utf8(&["add", "binary.ts"]).unwrap();
        repo.run_git_utf8(&["update-index", "--add", "--cacheinfo", &link])
            .unwrap();
        repo.run_git_utf8(&["commit", "-m", "test: add invalid source entries"])
            .unwrap();
        let rev = repo.resolve_revision("HEAD").unwrap();
        let paths = vec![
            "openapi.yaml".to_owned(),
            "binary.ts".to_owned(),
            "./link.ts".to_owned(),
        ];

        for snapshot in [rev.as_str(), STAGED_REV, WORKTREE_REV] {
            assert!(
                matches!(repo.read_file(snapshot, Path::new("binary.ts")), Err(LaineError::Io { source, .. }) if source.kind() == io::ErrorKind::InvalidData)
            );
            assert!(
                matches!(repo.read_file(snapshot, Path::new("link.ts")), Err(LaineError::Io { source, .. }) if source.kind() == io::ErrorKind::PermissionDenied)
            );
            let batch = repo.read_files_batch(snapshot, &paths).unwrap();
            assert_eq!(
                batch.keys().map(String::as_str).collect::<Vec<_>>(),
                ["openapi.yaml"]
            );
        }
        fs::remove_dir_all(&tmp).unwrap();
    }

    // Comprueba que un enlace a un directorio externo no sirva para abrir un archivo.
    #[cfg(unix)]
    #[test]
    fn confines_worktree_reads_below_the_checkout() {
        let tmp = std::env::temp_dir().join(format!("laine-git-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let paths = create_demo_workspace(&tmp).unwrap();
        fs::write(tmp.join("external.ts"), "external content").unwrap();
        std::os::unix::fs::symlink(&tmp, paths.customer_service.join("outside")).unwrap();
        let repo = GitRepo::new(&paths.customer_service);
        for path in ["outside/external.ts", "../external.ts"] {
            assert!(repo.read_file(WORKTREE_REV, Path::new(path)).is_err());
        }
        fs::remove_dir_all(&tmp).unwrap();
    }

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
