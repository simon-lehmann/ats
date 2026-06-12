//! CloneManager: template clones → cheap per-agent workspaces (plan §4.4).
//!
//! Copies use `cp --reflink=auto` on Unix (CoW where the filesystem supports
//! it, plain copy otherwise) and `robocopy` on Windows. Git operations shell
//! out — no libgit2 dependency for v1.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use ats_core::rpc::{Event, TemplateInfo, WorkspaceInfo};
use ats_core::state::WorkspaceStatus;
use tokio::process::Command;
use tokio::sync::broadcast;

use crate::store::Store;

pub struct CloneManager {
    store: Arc<Store>,
    events: broadcast::Sender<Event>,
    workspaces_root: PathBuf,
    data_dir: PathBuf,
}

async fn run(cmd: &mut Command, what: &str) -> Result<String> {
    let out = cmd.output().await.with_context(|| format!("running {what}"))?;
    // robocopy exits 0-7 on success; everything else uses 0 = ok
    let ok = if cfg!(windows) && what.starts_with("robocopy") {
        out.status.code().map(|c| c < 8).unwrap_or(false)
    } else {
        out.status.success()
    };
    if !ok {
        bail!(
            "{what} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    run(
        Command::new("git").arg("-C").arg(repo).args(args),
        &format!("git {}", args.join(" ")),
    )
    .await
}

async fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        run(
            Command::new("cp").arg("--reflink=auto").arg("-r").arg(src).arg(dst),
            "cp --reflink=auto",
        )
        .await?;
    }
    #[cfg(windows)]
    {
        run(
            Command::new("robocopy")
                .arg(src)
                .arg(dst)
                .args(["/E", "/MT", "/NFL", "/NDL", "/NJH", "/NJS"]),
            "robocopy",
        )
        .await?;
    }
    Ok(())
}

impl CloneManager {
    pub fn new(
        store: Arc<Store>,
        events: broadcast::Sender<Event>,
        workspaces_root: PathBuf,
        data_dir: PathBuf,
    ) -> Self {
        Self { store, events, workspaces_root, data_dir }
    }

    fn emit_status(&self, workspace_id: i64, status: WorkspaceStatus) {
        let _ = self.events.send(Event::WorkspaceStatusChanged { workspace_id, status });
    }

    pub async fn register_template(
        &self,
        name: &str,
        path: &str,
        setup_cmd: Option<&str>,
    ) -> Result<TemplateInfo> {
        let p = Path::new(path);
        if !p.join(".git").exists() {
            bail!("{path} is not a git repository (no .git)");
        }
        let origin = git(p, &["remote", "get-url", "origin"]).await.ok();
        self.store
            .insert_template(name, path, origin.as_deref().map(str::trim), setup_cmd)
    }

    pub async fn spawn_workspace(&self, template_id: i64) -> Result<WorkspaceInfo> {
        let template = self
            .store
            .get_template(template_id)?
            .ok_or_else(|| anyhow!("no template {template_id}"))?;
        let n = self.store.count_workspaces_for_template(template_id)? + 1;
        let dest = self.workspaces_root.join(format!("{}-{n}", template.name));
        if dest.exists() {
            bail!("workspace path {} already exists", dest.display());
        }
        std::fs::create_dir_all(&self.workspaces_root)?;

        let ws_id = self.store.insert_workspace(
            template_id,
            &dest.to_string_lossy(),
            WorkspaceStatus::Spawning,
        )?;
        self.emit_status(ws_id, WorkspaceStatus::Spawning);

        copy_tree(Path::new(&template.path), &dest).await?;
        let base_commit = git(&dest, &["rev-parse", "HEAD"]).await?.trim().to_string();
        let branch = format!("agent/{}-{n}", template.name);
        git(&dest, &["checkout", "-b", &branch]).await?;

        if let Some(setup) = &template.setup_cmd {
            #[cfg(unix)]
            run(Command::new("sh").arg("-c").arg(setup).current_dir(&dest), "setup_cmd").await?;
            #[cfg(windows)]
            run(Command::new("pwsh").args(["-NoProfile", "-Command", setup]).current_dir(&dest), "setup_cmd").await?;
        }

        self.store
            .update_workspace(ws_id, Some(&branch), Some(&base_commit), WorkspaceStatus::Ready)?;
        self.emit_status(ws_id, WorkspaceStatus::Ready);
        self.store
            .get_workspace(ws_id)?
            .ok_or_else(|| anyhow!("workspace vanished"))
    }

    pub async fn reset_workspace(&self, id: i64) -> Result<()> {
        let ws = self.store.get_workspace(id)?.ok_or_else(|| anyhow!("no workspace {id}"))?;
        let p = Path::new(&ws.path);
        git(p, &["reset", "--hard"]).await?;
        git(p, &["clean", "-fd"]).await?;
        self.store.update_workspace(id, None, None, WorkspaceStatus::Ready)?;
        self.emit_status(id, WorkspaceStatus::Ready);
        Ok(())
    }

    /// Diff the workspace against its spawn-time base commit; write a patch
    /// file under the daemon data dir and return (diffstat, patch path).
    pub async fn harvest_workspace(&self, id: i64) -> Result<(String, PathBuf)> {
        let ws = self.store.get_workspace(id)?.ok_or_else(|| anyhow!("no workspace {id}"))?;
        let base = self
            .store
            .workspace_base_commit(id)?
            .ok_or_else(|| anyhow!("workspace {id} has no base commit recorded"))?;
        let p = Path::new(&ws.path);
        self.emit_status(id, WorkspaceStatus::Harvesting);

        // include uncommitted work: diff base..worktree
        let stat = git(p, &["diff", "--stat", &base]).await?;
        let patch = git(p, &["diff", &base]).await?;

        let dir = self.data_dir.join("harvests");
        std::fs::create_dir_all(&dir)?;
        let patch_path = dir.join(format!("workspace-{id}.patch"));
        std::fs::write(&patch_path, &patch)?;

        self.store.update_workspace(id, None, None, WorkspaceStatus::Ready)?;
        self.emit_status(id, WorkspaceStatus::Ready);
        Ok((stat, patch_path))
    }

    pub async fn destroy_workspace(&self, id: i64) -> Result<()> {
        let ws = self.store.get_workspace(id)?.ok_or_else(|| anyhow!("no workspace {id}"))?;
        let p = PathBuf::from(&ws.path);
        // refuse to delete anything outside the workspaces root
        if !p.starts_with(&self.workspaces_root) {
            bail!(
                "refusing to delete {} — outside workspaces root {}",
                p.display(),
                self.workspaces_root.display()
            );
        }
        if p.exists() {
            tokio::fs::remove_dir_all(&p).await?;
        }
        self.store.update_workspace(id, None, None, WorkspaceStatus::Destroyed)?;
        self.emit_status(id, WorkspaceStatus::Destroyed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_template_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git_init(dir).await;
        std::fs::write(dir.join("README.md"), "# template\n").unwrap();
        git(dir, &["add", "-A"]).await.unwrap();
        git(dir, &["commit", "-m", "init"]).await.unwrap();
    }

    async fn git_init(dir: &Path) {
        run(Command::new("git").arg("-C").arg(dir).args(["init", "-b", "main"]), "git init")
            .await
            .unwrap();
        git(dir, &["config", "user.email", "t@t"]).await.unwrap();
        git(dir, &["config", "user.name", "t"]).await.unwrap();
    }

    fn mgr(root: &Path) -> CloneManager {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, _) = broadcast::channel(64);
        CloneManager::new(store, tx, root.join("workspaces"), root.join("data"))
    }

    #[tokio::test]
    async fn spawn_harvest_destroy_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let template_dir = tmp.path().join("template");
        make_template_repo(&template_dir).await;

        let m = mgr(tmp.path());
        let t = m
            .register_template("demo", &template_dir.to_string_lossy(), None)
            .await
            .unwrap();
        let ws = m.spawn_workspace(t.id).await.unwrap();
        assert_eq!(ws.status, WorkspaceStatus::Ready);
        assert_eq!(ws.branch.as_deref(), Some("agent/demo-1"));
        let ws_path = Path::new(&ws.path);
        assert!(ws_path.join("README.md").exists());

        // simulate agent work (uncommitted)
        std::fs::write(ws_path.join("new.rs"), "fn main() {}\n").unwrap();
        git(ws_path, &["add", "-A"]).await.unwrap();
        let (stat, patch) = m.harvest_workspace(ws.id).await.unwrap();
        assert!(stat.contains("new.rs"), "diffstat: {stat}");
        assert!(std::fs::read_to_string(&patch).unwrap().contains("fn main"));

        m.destroy_workspace(ws.id).await.unwrap();
        assert!(!ws_path.exists());
    }

    #[tokio::test]
    async fn register_rejects_non_git_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let m = mgr(tmp.path());
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(m
            .register_template("x", &plain.to_string_lossy(), None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn reset_discards_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let template_dir = tmp.path().join("template");
        make_template_repo(&template_dir).await;
        let m = mgr(tmp.path());
        let t = m
            .register_template("demo", &template_dir.to_string_lossy(), None)
            .await
            .unwrap();
        let ws = m.spawn_workspace(t.id).await.unwrap();
        let ws_path = Path::new(&ws.path);
        std::fs::write(ws_path.join("junk.txt"), "x").unwrap();
        m.reset_workspace(ws.id).await.unwrap();
        assert!(!ws_path.join("junk.txt").exists());
    }
}
