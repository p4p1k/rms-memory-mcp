use anyhow::Result;
use std::path::{Component, Path, PathBuf};

/// Validate that a path is relative, ends in `.md`, and does not escape the
/// vault root. Rejects absolute paths, `..` components, symlink traversal,
/// and non-Markdown extensions.
pub fn validate_path(path_str: &str) -> Result<()> {
    let path = Path::new(path_str);
    if path.is_absolute() {
        return Err(anyhow::anyhow!(
            "Path must be relative to the vault, but received absolute path: {}",
            path_str
        ));
    }
    for component in path.components() {
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            return Err(anyhow::anyhow!("Path must be relative to the vault"));
        }
        if component == Component::ParentDir {
            return Err(anyhow::anyhow!(
                "Path traversal detected: '..' is not allowed in vault paths"
            ));
        }
    }
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
    {
        return Err(anyhow::anyhow!(
            "Vault paths must point at Markdown (.md) documents: {}",
            path_str
        ));
    }
    Ok(())
}

/// Reject vault-relative paths inside the generated Wiki namespace. Wiki
/// pages have their own write channel (`WikiDocumentService`) that skips
/// audit metadata; the canonical MCP tools must never touch them.
pub fn reject_wiki_write(path_str: &str) -> Result<()> {
    if crate::path_policy::is_vault_wiki_relative_path(path_str) {
        return Err(anyhow::anyhow!(
            "Path '{path_str}' is inside the generated Wiki namespace and cannot be written through the canonical memory tools. Use the Wiki-specific write API instead.",
        ));
    }
    Ok(())
}

/// Resolve a relative path within `workspace_root` and validate it stays within bounds.
/// Also resolves symlinks to ensure the final path is within the vault.
pub fn resolve_vault_path(workspace_root: &Path, path_str: &str) -> Result<PathBuf> {
    validate_path(path_str)?;
    let joined = workspace_root.join(path_str);
    let canonical_root = std::fs::canonicalize(workspace_root)?;
    let mut ancestor = joined.as_path();
    let mut missing = Vec::new();
    let mut canonical = loop {
        match std::fs::canonicalize(ancestor) {
            Ok(path) => break path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // An existing but unresolvable entry (e.g. a dangling symlink)
                // must not be mistaken for a component that is safe to create.
                match std::fs::symlink_metadata(ancestor) {
                    Ok(_) => return Err(error.into()),
                    Err(metadata_error)
                        if metadata_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(metadata_error) => return Err(metadata_error.into()),
                }
                missing.push(
                    ancestor.file_name()
                        .ok_or_else(|| anyhow::anyhow!("Invalid vault path"))?
                        .to_os_string(),
                );
                ancestor = ancestor.parent()
                    .ok_or_else(|| anyhow::anyhow!("Invalid vault path"))?;
            }
            Err(error) => return Err(error.into()),
        }
    };
    if !canonical.starts_with(&canonical_root) {
        return Err(anyhow::anyhow!(
            "Path escapes vault boundary: resolved to outside workspace root"
        ));
    }
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_new_files_without_creating_directories() {
        let dir = tempfile::tempdir().unwrap();
        // Deliberately use an ordinary path, not the Windows verbatim path
        // returned by canonicalize: canonical roots hid the original bug.
        let root = ordinary_root(dir.path());
        std::fs::create_dir(root.join("decisions")).unwrap();
        for relative in ["new.md", "decisions/new.md", "missing/nested/new.md"] {
            let resolved = resolve_vault_path(&root, relative).unwrap();
            assert_eq!(resolved, std::fs::canonicalize(&root).unwrap().join(relative));
            assert!(!resolved.exists());
        }
        assert!(!root.join("missing").exists());
    }

    fn ordinary_root(path: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            let text = path.to_str().unwrap();
            if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{}", unc));
            }
            return PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text));
        }
        #[cfg(not(windows))]
        path.to_path_buf()
    }

    #[test]
    fn resolves_existing_file_and_rejects_invalid_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = ordinary_root(dir.path());
        std::fs::write(root.join("existing.md"), "unchanged").unwrap();
        assert_eq!(
            resolve_vault_path(&root, "existing.md").unwrap(),
            std::fs::canonicalize(root.join("existing.md")).unwrap()
        );
        assert!(resolve_vault_path(&root, "../outside.md").is_err());
        assert!(resolve_vault_path(&root, "a/../outside.md").is_err());
        assert!(resolve_vault_path(&root, root.join("absolute.md").to_str().unwrap()).is_err());
        assert_eq!(std::fs::read_to_string(root.join("existing.md")).unwrap(), "unchanged");
        #[cfg(windows)]
        for path in [r"decisions\new.md", r"missing\nested\new.md"] {
            assert!(resolve_vault_path(&root, path).is_ok());
        }
        #[cfg(windows)]
        for path in [r"C:outside.md", r"\outside.md", r"..\outside.md"] {
            assert!(resolve_vault_path(&root, path).is_err());
        }
    }

    fn assert_escape_rejected(root: &Path, outside: &Path) {
        for path in ["escape/existing.md", "escape/new.md", "escape/missing/new.md"] {
            let error = resolve_vault_path(root, path).unwrap_err();
            assert!(error.to_string().contains("Path escapes vault boundary"), "{error}");
        }
        assert_eq!(std::fs::read_to_string(outside.join("existing.md")).unwrap(), "sentinel");
        assert!(!outside.join("new.md").exists());
        assert!(!outside.join("missing").exists());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_junction_escape_for_existing_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = ordinary_root(dir.path());
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("existing.md"), "sentinel").unwrap();
        let status = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(root.join("escape"))
            .arg(ordinary_root(outside.path()))
            .status().unwrap();
        assert!(status.success(), "junction creation failed; test did not run");
        assert_escape_rejected(&root, outside.path());
        std::fs::remove_dir(root.join("escape")).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn rejects_symlink_escape_and_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = ordinary_root(dir.path());
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("existing.md"), "sentinel").unwrap();
        #[cfg(unix)]
        use std::os::unix::fs::symlink as symlink_dir;
        #[cfg(windows)]
        use std::os::windows::fs::symlink_dir;
        // On Windows this requires Developer Mode or symlink privilege.
        // Fail explicitly rather than report a skipped security test as passed.
        symlink_dir(outside.path(), root.join("escape")).expect("symlink privilege required");
        assert_escape_rejected(&root, outside.path());
        symlink_dir(outside.path().join("absent"), root.join("dangling.md")).unwrap();
        assert!(resolve_vault_path(&root, "dangling.md").is_err());
        assert!(resolve_vault_path(&root, "dangling.md/new.md").is_err());
        #[cfg(windows)]
        {
            std::fs::remove_dir(root.join("escape")).unwrap();
            std::fs::remove_dir(root.join("dangling.md")).unwrap();
        }
        #[cfg(unix)]
        {
            std::fs::remove_file(root.join("escape")).unwrap();
            std::fs::remove_file(root.join("dangling.md")).unwrap();
        }
    }

    #[test]
    fn validate_path_requires_markdown_extension() {
        assert!(validate_path("rules/api.md").is_ok());
        assert!(validate_path("Notes/2024-01.MD").is_ok());
        let error = validate_path("secret.txt").unwrap_err().to_string();
        assert!(error.contains("Markdown"));
        assert!(validate_path("no-extension").is_err());
    }

    #[test]
    fn reject_wiki_write_blocks_wiki_paths() {
        assert!(reject_wiki_write("wiki/index.md").is_err());
        assert!(reject_wiki_write("./Wiki/guide.md").is_err());
        assert!(reject_wiki_write("wiki\\page.md").is_err());
        assert!(reject_wiki_write("docs/wiki.md").is_ok());
    }
}
