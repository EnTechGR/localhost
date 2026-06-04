/// Safe filesystem path resolution.
///
/// Prevents directory traversal attacks by canonicalising the resolved path
/// and verifying it still begins with the route root.
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum PathError {
    /// The resolved path escapes the root directory (`../` traversal).
    Traversal,
    /// The root path could not be canonicalised (does not exist).
    InvalidRoot(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::Traversal        => write!(f, "path traversal detected"),
            PathError::InvalidRoot(r)   => write!(f, "invalid root: {r}"),
        }
    }
}

impl std::error::Error for PathError {}

/// Resolve `url_path` against `root`, blocking directory traversal.
///
/// Steps:
/// 1. Join `root` + `url_path` (treating `url_path` as a relative suffix).
/// 2. Canonicalise: resolve `..`, symlinks, etc.
/// 3. Verify the canonical result has `canonical_root` as a prefix.
///
/// Returns the safe `PathBuf`, or `PathError::Traversal` if the resolved
/// path escapes the root. Returns `PathError::InvalidRoot` if `root` itself
/// does not exist on disk.
///
/// Note: the canonicalisation step requires the path to exist on disk.
/// Use `resolve_safe_virtual` (below) for paths that may not exist yet
/// (e.g. upload targets).
pub fn resolve_safe(root: &str, url_path: &str) -> Result<PathBuf, PathError> {
    let canonical_root = Path::new(root)
        .canonicalize()
        .map_err(|_| PathError::InvalidRoot(root.to_string()))?;

    // Strip leading '/' from url_path so Path::join doesn't treat it as
    // absolute (which would discard the root entirely).
    let stripped = url_path.trim_start_matches('/');
    let joined   = canonical_root.join(stripped);

    // canonicalize() resolves ".." and symlinks, returning the real path.
    // If the joined path does not exist, we can't canonicalize — fall back
    // to the lexical check.
    let canonical_joined = joined.canonicalize().unwrap_or_else(|_| {
        // Path doesn't exist yet — do lexical normalisation.
        lexical_normalize(&joined)
    });

    if !canonical_joined.starts_with(&canonical_root) {
        return Err(PathError::Traversal);
    }

    Ok(canonical_joined)
}

/// Lexically normalise a path (resolve `.` and `..` components) without
/// requiring the path to exist on disk.
///
/// Used for upload paths and for paths that will be created.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => { out.pop(); }
            std::path::Component::CurDir    => {}
            other => out.push(other),
        }
    }
    out
}

/// Resolve `url_path` against `root` without requiring the path to exist.
///
/// Uses lexical normalisation only. Suitable for checking upload destinations
/// before the file exists. Less secure than `resolve_safe` for existing paths
/// because symlinks are not resolved.
pub fn resolve_safe_virtual(root: &str, url_path: &str) -> Result<PathBuf, PathError> {
    let root_path = Path::new(root);
    let stripped  = url_path.trim_start_matches('/');
    let joined    = root_path.join(stripped);
    let normalised = lexical_normalize(&joined);
    let root_norm  = lexical_normalize(root_path);

    if !normalised.starts_with(&root_norm) {
        return Err(PathError::Traversal);
    }

    Ok(normalised)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Helper: create a temp dir with a subdir and file.
    fn with_tmp<F: FnOnce(&str)>(f: F) {
        let dir = format!("/tmp/path_test_{}", unsafe { libc::getpid() });
        fs::create_dir_all(&dir).unwrap();
        f(&dir);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_safe_normal_path() {
        with_tmp(|root| {
            let file = format!("{root}/index.html");
            fs::write(&file, b"hi").unwrap();
            let result = resolve_safe(root, "/index.html").unwrap();
            assert_eq!(result, std::path::Path::new(&file).canonicalize().unwrap());
        });
    }

    #[test]
    fn resolve_safe_blocks_traversal() {
        with_tmp(|root| {
            // Create a file outside the root to traverse to.
            let outside = format!("{root}/../outside.txt");
            // We don't need it to exist — the resolver should block it.
            let result = resolve_safe(root, "/../etc/passwd");
            assert!(result.is_err(), "should have blocked traversal");
        });
    }

    #[test]
    fn resolve_safe_blocks_dotdot_in_path() {
        with_tmp(|root| {
            let result = resolve_safe(root, "/subdir/../../etc/passwd");
            assert!(result.is_err());
        });
    }

    #[test]
    fn resolve_safe_allows_subdirectory() {
        with_tmp(|root| {
            fs::create_dir_all(format!("{root}/static")).unwrap();
            fs::write(format!("{root}/static/app.js"), b"").unwrap();
            let result = resolve_safe(root, "/static/app.js");
            assert!(result.is_ok());
        });
    }

    #[test]
    fn resolve_safe_virtual_blocks_traversal() {
        let result = resolve_safe_virtual("/var/www", "/../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_safe_virtual_allows_normal() {
        let result = resolve_safe_virtual("/var/www", "/static/app.js");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), PathBuf::from("/var/www/static/app.js"));
    }

    #[test]
    fn lexical_normalize_resolves_dotdot() {
        let p = lexical_normalize(Path::new("/a/b/../c"));
        assert_eq!(p, PathBuf::from("/a/c"));
    }

    #[test]
    fn lexical_normalize_resolves_dot() {
        let p = lexical_normalize(Path::new("/a/./b"));
        assert_eq!(p, PathBuf::from("/a/b"));
    }
}