use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, ensure};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories,
};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::Workspace;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub operation_id: String,
    pub commit_id: String,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkChange {
    pub operation_id: String,
    pub commit_id: String,
    pub parent_commit_id: String,
    pub description: String,
}

pub struct JjWorkspace {
    root: PathBuf,
    workspace: Workspace,
    repo: Arc<ReadonlyRepo>,
}

fn settings() -> Result<UserSettings> {
    let mut config = StackedConfig::with_defaults();
    config.add_layer(ConfigLayer::parse(
        ConfigSource::User,
        r#"
            [user]
            name = "JujuLeaf"
            email = "sync@jujuleaf.local"

            [operation]
            hostname = "jujuleaf"
            username = "jujuleaf"
        "#,
    )?);
    UserSettings::from_config(config).map_err(Into::into)
}

impl JjWorkspace {
    pub async fn init(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        std::fs::create_dir_all(root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        let user_settings = settings()?;
        let (workspace, repo) =
            Workspace::init_internal_git(&user_settings, root, gix::hash::Kind::Sha1)
                .await
                .context("failed to initialize Jujutsu workspace")?;
        Ok(Self {
            root: root.to_owned(),
            workspace,
            repo,
        })
    }

    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let user_settings = settings()?;
        let workspace = Workspace::load(
            &user_settings,
            root,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .context("failed to load Jujutsu workspace")?;
        let repo = workspace
            .repo_loader()
            .load_at_head()
            .await
            .context("failed to load Jujutsu operation head")?;
        Ok(Self {
            root: root.to_owned(),
            workspace,
            repo,
        })
    }

    pub async fn init_or_open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if root.join(".jj").exists() {
            Self::open(root).await
        } else {
            Self::init(root).await
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn operation_id(&self) -> String {
        self.repo.op_id().hex()
    }

    pub async fn checkpoint(&mut self, description: &str) -> Result<Checkpoint> {
        let workspace_name = self.workspace.workspace_name().to_owned();
        let old_commit_id = self
            .repo
            .view()
            .get_wc_commit_id(&workspace_name)
            .cloned()
            .ok_or_else(|| anyhow!("Jujutsu workspace has no working-copy commit"))?;
        let old_commit = self
            .repo
            .store()
            .get_commit_async(&old_commit_id)
            .await
            .context("failed to load Jujutsu working-copy commit")?;

        let mut locked_workspace = self
            .workspace
            .start_working_copy_mutation()
            .await
            .context("failed to lock Jujutsu working copy")?;
        let snapshot_options = SnapshotOptions {
            base_ignores: GitIgnoreFile::empty(),
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &EverythingMatcher,
            max_new_file_size: 100 * 1024 * 1024,
        };
        let (tree, _) = locked_workspace
            .locked_wc()
            .snapshot(&snapshot_options)
            .await
            .context("failed to snapshot JujuLeaf files")?;
        let changed = tree.tree_ids_and_labels() != old_commit.tree().tree_ids_and_labels();
        if !changed {
            locked_workspace
                .finish(self.repo.op_id().clone())
                .await
                .context("failed to persist Jujutsu working-copy state")?;
            return Ok(Checkpoint {
                operation_id: self.repo.op_id().hex(),
                commit_id: old_commit.id().hex(),
                changed: false,
            });
        }

        let mut transaction = self.repo.start_transaction();
        transaction.set_is_snapshot(true);
        transaction.set_workspace_name(&workspace_name);
        let new_commit = transaction
            .repo_mut()
            .rewrite_commit(&old_commit)
            .set_tree(tree)
            .set_description(description)
            .write()
            .await
            .context("failed to write Jujutsu commit")?;
        transaction
            .repo_mut()
            .set_wc_commit(workspace_name, new_commit.id().clone())?;
        transaction.repo_mut().rebase_descendants().await?;
        let repo = transaction
            .commit(format!("jujuleaf checkpoint: {description}"))
            .await
            .context("failed to publish Jujutsu operation")?;
        locked_workspace
            .finish(repo.op_id().clone())
            .await
            .context("failed to persist Jujutsu working-copy state")?;

        let checkpoint = Checkpoint {
            operation_id: repo.op_id().hex(),
            commit_id: new_commit.id().hex(),
            changed,
        };
        self.repo = repo;
        Ok(checkpoint)
    }

    /// Snapshot the current working copy and start a described child change.
    ///
    /// Unlike `checkpoint`, this creates a stable work boundary: later file
    /// snapshots rewrite the new child instead of the synchronized parent.
    pub async fn begin_change(&mut self, description: &str) -> Result<WorkChange> {
        let description = description.trim();
        ensure!(!description.is_empty(), "work description cannot be empty");
        self.checkpoint("capture local state before beginning work")
            .await?;

        let workspace_name = self.workspace.workspace_name().to_owned();
        let parent_commit_id = self
            .repo
            .view()
            .get_wc_commit_id(&workspace_name)
            .cloned()
            .ok_or_else(|| anyhow!("Jujutsu workspace has no working-copy commit"))?;
        let parent_commit = self
            .repo
            .store()
            .get_commit_async(&parent_commit_id)
            .await
            .context("failed to load Jujutsu working-copy commit")?;

        let mut locked_workspace = self
            .workspace
            .start_working_copy_mutation()
            .await
            .context("failed to lock Jujutsu working copy")?;
        let mut transaction = self.repo.start_transaction();
        transaction.set_workspace_name(&workspace_name);
        let new_commit = transaction
            .repo_mut()
            .new_commit(vec![parent_commit.id().clone()], parent_commit.tree())
            .set_description(description)
            .write()
            .await
            .context("failed to create Jujutsu work change")?;
        transaction
            .repo_mut()
            .set_wc_commit(workspace_name, new_commit.id().clone())?;
        let repo = transaction
            .commit(format!("jujuleaf begin: {description}"))
            .await
            .context("failed to publish Jujutsu work change")?;
        locked_workspace
            .locked_wc()
            .check_out(&new_commit)
            .await
            .context("failed to check out Jujutsu work change")?;
        locked_workspace.finish(repo.op_id().clone()).await?;

        let change = WorkChange {
            operation_id: repo.op_id().hex(),
            commit_id: new_commit.id().hex(),
            parent_commit_id: parent_commit.id().hex(),
            description: description.to_owned(),
        };
        self.repo = repo;
        Ok(change)
    }

    /// Snapshot pending files and set the current work change description.
    pub async fn describe_change(&mut self, description: &str) -> Result<WorkChange> {
        let description = description.trim();
        ensure!(!description.is_empty(), "work description cannot be empty");
        self.checkpoint(description).await?;

        let workspace_name = self.workspace.workspace_name().to_owned();
        let current_commit_id = self
            .repo
            .view()
            .get_wc_commit_id(&workspace_name)
            .cloned()
            .ok_or_else(|| anyhow!("Jujutsu workspace has no working-copy commit"))?;
        let current_commit = self
            .repo
            .store()
            .get_commit_async(&current_commit_id)
            .await?;
        let parent_commit_id = current_commit
            .parent_ids()
            .first()
            .map(ObjectId::hex)
            .unwrap_or_default();
        if current_commit.description() == description {
            return Ok(WorkChange {
                operation_id: self.repo.op_id().hex(),
                commit_id: current_commit.id().hex(),
                parent_commit_id,
                description: description.to_owned(),
            });
        }

        let locked_workspace = self
            .workspace
            .start_working_copy_mutation()
            .await
            .context("failed to lock Jujutsu working copy")?;
        let mut transaction = self.repo.start_transaction();
        transaction.set_workspace_name(&workspace_name);
        let new_commit = transaction
            .repo_mut()
            .rewrite_commit(&current_commit)
            .set_description(description)
            .write()
            .await
            .context("failed to describe Jujutsu work change")?;
        transaction
            .repo_mut()
            .set_wc_commit(workspace_name, new_commit.id().clone())?;
        transaction.repo_mut().rebase_descendants().await?;
        let repo = transaction
            .commit(format!("jujuleaf describe: {description}"))
            .await?;
        locked_workspace.finish(repo.op_id().clone()).await?;

        let change = WorkChange {
            operation_id: repo.op_id().hex(),
            commit_id: new_commit.id().hex(),
            parent_commit_id,
            description: description.to_owned(),
        };
        self.repo = repo;
        Ok(change)
    }

    /// Abandon the current described child and restore its synchronized parent.
    pub async fn abandon_change(&mut self, expected_parent_commit_id: &str) -> Result<Checkpoint> {
        self.checkpoint("capture local state before aborting review")
            .await?;

        let workspace_name = self.workspace.workspace_name().to_owned();
        let current_commit_id = self
            .repo
            .view()
            .get_wc_commit_id(&workspace_name)
            .cloned()
            .ok_or_else(|| anyhow!("Jujutsu workspace has no working-copy commit"))?;
        let current_commit = self
            .repo
            .store()
            .get_commit_async(&current_commit_id)
            .await?;
        let parent_commit_id = current_commit
            .parent_ids()
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("current work change has no parent"))?;
        ensure!(
            parent_commit_id.hex() == expected_parent_commit_id,
            "current Jujutsu work is no longer the active review change"
        );
        let parent_commit = self
            .repo
            .store()
            .get_commit_async(&parent_commit_id)
            .await?;

        let mut locked_workspace = self
            .workspace
            .start_working_copy_mutation()
            .await
            .context("failed to lock Jujutsu working copy")?;
        let mut transaction = self.repo.start_transaction();
        transaction.set_workspace_name(&workspace_name);
        transaction
            .repo_mut()
            .record_abandoned_commit(&current_commit);
        transaction
            .repo_mut()
            .set_wc_commit(workspace_name, parent_commit_id)?;
        transaction.repo_mut().rebase_descendants().await?;
        let repo = transaction
            .commit("jujuleaf review abort")
            .await
            .context("failed to abandon Jujutsu review change")?;
        locked_workspace
            .locked_wc()
            .check_out(&parent_commit)
            .await
            .context("failed to restore synchronized parent")?;
        locked_workspace.finish(repo.op_id().clone()).await?;

        let checkpoint = Checkpoint {
            operation_id: repo.op_id().hex(),
            commit_id: parent_commit.id().hex(),
            changed: current_commit.tree().tree_ids_and_labels()
                != parent_commit.tree().tree_ids_and_labels(),
        };
        self.repo = repo;
        Ok(checkpoint)
    }

    async fn restore_operation(
        &mut self,
        target_operation: &jj_lib::operation::Operation,
        description: &str,
    ) -> Result<Checkpoint> {
        let workspace_name = self.workspace.workspace_name().to_owned();
        let target_repo = self
            .repo
            .loader()
            .load_at(target_operation)
            .await
            .context("failed to load target Jujutsu operation")?;
        let target_commit_id = target_repo
            .view()
            .get_wc_commit_id(&workspace_name)
            .cloned()
            .ok_or_else(|| anyhow!("target operation has no working-copy commit"))?;
        let target_commit = target_repo
            .store()
            .get_commit_async(&target_commit_id)
            .await?;
        let current_commit_id = self
            .repo
            .view()
            .get_wc_commit_id(&workspace_name)
            .cloned()
            .ok_or_else(|| anyhow!("current operation has no working-copy commit"))?;
        let current_commit = self
            .repo
            .store()
            .get_commit_async(&current_commit_id)
            .await?;

        let mut locked_workspace = self
            .workspace
            .start_working_copy_mutation()
            .await
            .context("failed to lock Jujutsu working copy")?;
        let mut transaction = self.repo.start_transaction();
        transaction.set_workspace_name(&workspace_name);
        transaction.set_attribute(
            "jujuleaf.restore-operation".to_owned(),
            target_operation.id().hex(),
        );
        let new_commit = transaction
            .repo_mut()
            .rewrite_commit(&current_commit)
            .set_tree(target_commit.tree())
            .set_description(description)
            .write()
            .await?;
        transaction
            .repo_mut()
            .set_wc_commit(workspace_name, new_commit.id().clone())?;
        transaction.repo_mut().rebase_descendants().await?;
        let repo = transaction.commit(description).await?;
        locked_workspace
            .locked_wc()
            .check_out(&new_commit)
            .await
            .context("failed to restore files from Jujutsu")?;
        locked_workspace.finish(repo.op_id().clone()).await?;
        let checkpoint = Checkpoint {
            operation_id: repo.op_id().hex(),
            commit_id: new_commit.id().hex(),
            changed: target_commit.tree().tree_ids_and_labels()
                != current_commit.tree().tree_ids_and_labels(),
        };
        self.repo = repo;
        Ok(checkpoint)
    }

    pub async fn undo(&mut self) -> Result<Checkpoint> {
        self.checkpoint("capture local state before undo").await?;
        let current_operation_id = self.repo.op_id().hex();
        let parent = self
            .repo
            .operation()
            .parents()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no earlier Jujutsu operation to undo to"))?;
        let redo_path = self.root.join(".jj/jujuleaf/redo-operation");
        if let Some(parent) = redo_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&redo_path, current_operation_id)?;
        self.restore_operation(&parent, "jujuleaf undo").await
    }

    pub async fn redo(&mut self) -> Result<Checkpoint> {
        let redo_path = self.root.join(".jj/jujuleaf/redo-operation");
        let operation_id = std::fs::read_to_string(&redo_path).context("nothing to redo")?;
        let operation_id = jj_lib::op_store::OperationId::try_from_hex(operation_id.trim())
            .ok_or_else(|| anyhow!("invalid saved redo operation ID"))?;
        let operation = self
            .repo
            .loader()
            .load_operation(&operation_id)
            .await
            .context("saved redo operation no longer exists")?;
        let checkpoint = self.restore_operation(&operation, "jujuleaf redo").await?;
        std::fs::remove_file(redo_path).ok();
        Ok(checkpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initializes_and_checkpoints_a_real_jj_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = JjWorkspace::init(temp.path()).await.unwrap();
        assert!(temp.path().join(".jj").is_dir());

        std::fs::write(temp.path().join("main.tex"), "first").unwrap();
        let first = workspace.checkpoint("pull remote version 1").await.unwrap();
        assert!(first.changed);
        assert!(!first.operation_id.is_empty());
        assert!(!first.commit_id.is_empty());

        std::fs::write(temp.path().join("main.tex"), "second").unwrap();
        let second = workspace.checkpoint("local edit").await.unwrap();
        assert!(second.changed);
        assert_ne!(first.operation_id, second.operation_id);
        assert_ne!(first.commit_id, second.commit_id);

        let reopened = JjWorkspace::open(temp.path()).await.unwrap();
        assert_eq!(reopened.operation_id(), second.operation_id);
    }

    #[tokio::test]
    async fn undo_and_redo_restore_working_copy_content() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = JjWorkspace::init(temp.path()).await.unwrap();
        let file = temp.path().join("main.tex");
        std::fs::write(&file, "one").unwrap();
        workspace.checkpoint("one").await.unwrap();
        std::fs::write(&file, "two").unwrap();
        workspace.checkpoint("two").await.unwrap();

        let undone = workspace.undo().await.unwrap();
        assert!(undone.changed);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one");
        let redone = workspace.redo().await.unwrap();
        assert!(redone.changed);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
    }

    #[tokio::test]
    async fn begin_creates_a_described_child_work_change() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = JjWorkspace::init(temp.path()).await.unwrap();
        let file = temp.path().join("main.tex");
        std::fs::write(&file, "base").unwrap();
        let base = workspace.checkpoint("synchronized base").await.unwrap();

        let work = workspace
            .begin_change("rewrite introduction")
            .await
            .unwrap();
        assert_eq!(work.parent_commit_id, base.commit_id);
        assert_ne!(work.commit_id, base.commit_id);
        assert_eq!(work.description, "rewrite introduction");

        std::fs::write(&file, "draft").unwrap();
        let draft = workspace.checkpoint("rewrite introduction").await.unwrap();
        assert!(draft.changed);
        assert_ne!(draft.commit_id, work.commit_id);
    }

    #[tokio::test]
    async fn abandon_change_restores_the_synchronized_parent() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = JjWorkspace::init(temp.path()).await.unwrap();
        let file = temp.path().join("main.tex");
        std::fs::write(&file, "base").unwrap();
        let base = workspace.checkpoint("synchronized base").await.unwrap();
        let work = workspace.begin_change("draft").await.unwrap();

        std::fs::write(&file, "unsubmitted draft").unwrap();
        let aborted = workspace
            .abandon_change(&work.parent_commit_id)
            .await
            .unwrap();
        assert!(aborted.changed);
        assert_eq!(aborted.commit_id, base.commit_id);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "base");

        let next = workspace.begin_change("next work").await.unwrap();
        assert_eq!(next.parent_commit_id, base.commit_id);
    }
}
