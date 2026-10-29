use super::AppContext;
use anyhow::Result;

pub async fn execute(
    ctx: &AppContext,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value> {
    // `noPromote` is reserved for read-without-side-effects. Vault reads are
    // already side-effect free (no access counters); the flag is accepted so
    // agents/clients can request that contract explicitly and stay forward-compatible.
    let _no_promote = args
        .get("noPromote")
        .or_else(|| args.get("no_promote"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let workspace_root = ctx
        .workspace_root
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Workspace root not initialized"))?;
    let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let file_path = super::validation::resolve_vault_path(workspace_root, path_str)?;

    // If this document is a link, fetch the linked content — but only when the
    // resolved target stays inside the vault. Escapes fall through to reading
    // the link file itself instead of leaking data from outside the vault.
    if let Some(linked_content) = crate::link::get_linked_content_in_vault(
        &file_path,
        workspace_root,
        ctx.code_path.as_deref(),
    ) {
        Ok(super::response::json_text_response(&linked_content))
    } else {
        // Markdown is authoritative. The derived index can lag a successful
        // write, omit frontmatter, or contain a deleted document's old chunks.
        let full_text = std::fs::read_to_string(&file_path)?;
        Ok(super::response::json_text_response(&full_text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context(root: &std::path::Path) -> AppContext {
        AppContext {
            store: None, indexer: None,
            workspace_root: Some(std::fs::canonicalize(root).unwrap()),
            code_path: None, max_backups: 0, scope: None,
            caller_id: "read-regression".into(), project_key: None,
        }
    }

    #[tokio::test]
    async fn read_immediately_after_write_without_index_returns_complete_file() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let args = json!({"path":"decisions/new.md", "mode":"create",
            "content":"# RMS Memory Test\nКонтрольный код: RMS-2710.", "dry_run":false});
        super::super::write::execute(&ctx, args.as_object().unwrap()).await.unwrap();
        let read = execute(&ctx, args.as_object().unwrap()).await.unwrap();
        let expected = std::fs::read_to_string(dir.path().join("decisions/new.md")).unwrap();
        assert_eq!(read["content"][0]["text"].as_str(), Some(expected.as_str()));
        assert!(expected.contains("RMS-2710"));
        assert!(expected.starts_with("---\n"));
    }

    #[tokio::test]
    async fn read_observes_replacement_and_missing_file_without_stale_index_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let path = dir.path().join("note.md");
        let args = json!({"path":"note.md"});
        std::fs::write(&path, "---\nid: current\n---\n# Current\n").unwrap();
        let read = execute(&ctx, args.as_object().unwrap()).await.unwrap();
        assert_eq!(read["content"][0]["text"], "---\nid: current\n---\n# Current\n");
        std::fs::write(&path, "# Replaced\n").unwrap();
        let read = execute(&ctx, args.as_object().unwrap()).await.unwrap();
        assert_eq!(read["content"][0]["text"], "# Replaced\n");
        std::fs::remove_file(path).unwrap();
        assert!(execute(&ctx, args.as_object().unwrap()).await.is_err());
    }

    #[tokio::test]
    async fn read_rejects_parent_escape() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        std::fs::write(dir.path().join("outside.md"), "private").unwrap();
        let args = json!({"path":"../outside.md"});
        assert!(execute(&context(&vault), args.as_object().unwrap()).await.is_err());
    }

    #[tokio::test]
    async fn read_preserves_safe_link_documents_and_never_reads_external_link_content() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        std::fs::write(vault.join("source.md"), "# Linked content\n").unwrap();
        std::fs::write(vault.join("link.md"), "---\nlink: source.md\n---\nplaceholder").unwrap();
        let args = json!({"path":"link.md"});
        let read = execute(&context(&vault), args.as_object().unwrap()).await.unwrap();
        assert_eq!(read["content"][0]["text"], "# Linked content\n");
        std::fs::write(dir.path().join("outside.md"), "external private content").unwrap();
        let link = "---\nlink: ../outside.md\n---\nplaceholder";
        std::fs::write(vault.join("link.md"), link).unwrap();
        let read = execute(&context(&vault), args.as_object().unwrap()).await.unwrap();
        assert_eq!(read["content"][0]["text"], link);
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn read_rejects_filesystem_link_outside_vault() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        std::fs::create_dir(&vault).unwrap();
        let outside = dir.path().join("outside.md");
        std::fs::write(&outside, "external private content").unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside, vault.join("escape.md")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, vault.join("escape.md")).unwrap();
        let args = json!({"path":"escape.md"});
        assert!(execute(&context(&vault), args.as_object().unwrap()).await.is_err());
    }
}
