use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, ensure};
use futures_util::{AsyncReadExt, StreamExt};
use jj_lib::backend::{MergedTreeValue, TreeValue};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories,
};
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::operation::Operation;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::Workspace;
use serde::Serialize;
use similar::TextDiff;

use crate::ignore;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalHistoryEntry {
    pub operation_id: String,
    pub commit_id: String,
    pub change_id: String,
    pub description: String,
    pub commit_description: String,
    pub timestamp: String,
    pub current: bool,
    pub snapshot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalHistorySummary {
    pub current_operation_id: String,
    pub entries: Vec<LocalHistoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalFileChange {
    pub path: String,
    pub change: String,
    pub binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalChangeSummary {
    pub operation_id: String,
    pub commit_id: String,
    pub change_id: String,
    pub description: String,
    pub commit_description: String,
    pub timestamp: String,
    pub files: Vec<LocalFileChange>,
    pub patch: String,
}

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
        let base_ignores = ignore::load(&self.root)?;
        let snapshot_options = SnapshotOptions {
            base_ignores,
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &NothingMatcher,
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

    async fn operation_commit(
        &self,
        operation: &Operation,
    ) -> Result<(Arc<ReadonlyRepo>, jj_lib::commit::Commit)> {
        let repo = self
            .repo
            .loader()
            .load_at(operation)
            .await
            .context("failed to load Jujutsu operation")?;
        let commit_id = repo
            .view()
            .get_wc_commit_id(self.workspace.workspace_name())
            .cloned()
            .ok_or_else(|| anyhow!("operation has no working-copy commit"))?;
        let commit = repo.store().get_commit_async(&commit_id).await?;
        Ok((repo, commit))
    }

    async fn history_operations(&self, limit: Option<usize>) -> Result<Vec<Operation>> {
        let mut operations = Vec::new();
        let mut operation = self.repo.operation().clone();
        loop {
            if operation.parent_ids().is_empty() {
                break;
            }
            operations.push(operation.clone());
            if limit.is_some_and(|limit| operations.len() >= limit) {
                break;
            }
            operation = operation
                .parents()
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("broken Jujutsu operation history"))?;
        }
        Ok(operations)
    }

    async fn resolve_operation(&self, revision: &str) -> Result<Operation> {
        if revision == "@" {
            return Ok(self.repo.operation().clone());
        }
        ensure!(
            revision.len() >= 4 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "local revision must be '@' or a hexadecimal ID prefix of at least 4 characters"
        );
        let mut matches = Vec::new();
        for operation in self.history_operations(None).await? {
            let (_, commit) = self.operation_commit(&operation).await?;
            if operation.id().hex().starts_with(revision) || commit.id().hex().starts_with(revision)
            {
                matches.push(operation);
            }
        }
        ensure!(!matches.is_empty(), "local revision not found: {revision}");
        ensure!(
            matches.len() == 1,
            "ambiguous local revision '{revision}'; provide a longer ID"
        );
        Ok(matches.remove(0))
    }

    pub async fn history(&self, limit: usize) -> Result<LocalHistorySummary> {
        ensure!(limit > 0, "--limit must be greater than zero");
        ensure!(limit <= 1_000, "--limit cannot exceed 1000");
        let current_operation_id = self.repo.op_id().hex();
        let mut entries = Vec::new();
        for operation in self.history_operations(Some(limit)).await? {
            let (_, commit) = self.operation_commit(&operation).await?;
            let timestamp = operation
                .metadata()
                .time
                .end
                .to_datetime()
                .context("operation timestamp is out of range")?
                .to_rfc3339();
            entries.push(LocalHistoryEntry {
                operation_id: operation.id().hex(),
                commit_id: commit.id().hex(),
                change_id: commit.change_id().hex(),
                description: operation.metadata().description.clone(),
                commit_description: commit.description().to_owned(),
                timestamp,
                current: operation.id().hex() == current_operation_id,
                snapshot: operation.metadata().is_snapshot,
            });
        }
        Ok(LocalHistorySummary {
            current_operation_id,
            entries,
        })
    }

    async fn value_bytes(
        store: &Arc<Store>,
        path: &jj_lib::repo_path::RepoPath,
        value: MergedTreeValue,
    ) -> Result<(bool, Option<Vec<u8>>)> {
        let value = value.into_resolved().map_err(|_| {
            anyhow!(
                "unresolved Jujutsu tree conflict at {}",
                path.as_internal_file_string()
            )
        })?;
        match value {
            None => Ok((false, None)),
            Some(TreeValue::File { id, .. }) => {
                let mut reader = store.read_file(path, &id).await?;
                let mut contents = Vec::new();
                reader.read_to_end(&mut contents).await?;
                Ok((true, Some(contents)))
            }
            Some(_) => Ok((true, None)),
        }
    }

    async fn diff_trees(
        &self,
        before: &MergedTree,
        after: &MergedTree,
        include_internal: bool,
    ) -> Result<(Vec<LocalFileChange>, String)> {
        let matcher = EverythingMatcher;
        let mut stream = before.diff_stream(after, &matcher);
        let mut files = Vec::new();
        let mut patch = String::new();
        while let Some(entry) = stream.next().await {
            let path = entry.path.as_internal_file_string().to_owned();
            if !include_internal && (path == ".jujuleaf" || path.starts_with(".jujuleaf/")) {
                continue;
            }
            let values = entry.values?;
            let (before_present, before_bytes) =
                Self::value_bytes(before.store(), &entry.path, values.before).await?;
            let (after_present, after_bytes) =
                Self::value_bytes(after.store(), &entry.path, values.after).await?;
            let change = match (before_present, after_present) {
                (false, true) => "added",
                (true, false) => "deleted",
                (true, true) => "modified",
                (false, false) => continue,
            }
            .to_owned();
            let before_text = before_bytes
                .as_deref()
                .map(std::str::from_utf8)
                .transpose()
                .ok()
                .flatten();
            let after_text = after_bytes
                .as_deref()
                .map(std::str::from_utf8)
                .transpose()
                .ok()
                .flatten();
            let binary = (before_bytes.is_some() && before_text.is_none())
                || (after_bytes.is_some() && after_text.is_none())
                || before_bytes.is_none() && before_present
                || after_bytes.is_none() && after_present;
            if !binary {
                let old = before_text.unwrap_or_default();
                let new = after_text.unwrap_or_default();
                let rendered = TextDiff::from_lines(old, new)
                    .unified_diff()
                    .context_radius(3)
                    .header(&format!("a/{path}"), &format!("b/{path}"))
                    .to_string();
                if !rendered.is_empty() {
                    if !patch.is_empty() {
                        patch.push('\n');
                    }
                    patch.push_str(&rendered);
                }
            }
            files.push(LocalFileChange {
                path,
                change,
                binary,
            });
        }
        Ok((files, patch))
    }

    async fn operation_change(
        &self,
        operation: &Operation,
        include_internal: bool,
    ) -> Result<LocalChangeSummary> {
        let (repo, commit) = self.operation_commit(operation).await?;
        let before_tree = if let Some(parent) = operation.parents().await?.into_iter().next() {
            match self.operation_commit(&parent).await {
                Ok((_, parent_commit)) => parent_commit.tree(),
                Err(_) => repo.store().empty_merged_tree(),
            }
        } else {
            repo.store().empty_merged_tree()
        };
        let (files, patch) = self
            .diff_trees(&before_tree, &commit.tree(), include_internal)
            .await?;
        let timestamp = operation
            .metadata()
            .time
            .end
            .to_datetime()
            .context("operation timestamp is out of range")?
            .to_rfc3339();
        Ok(LocalChangeSummary {
            operation_id: operation.id().hex(),
            commit_id: commit.id().hex(),
            change_id: commit.change_id().hex(),
            description: operation.metadata().description.clone(),
            commit_description: commit.description().to_owned(),
            timestamp,
            files,
            patch,
        })
    }

    pub async fn show(&self, revision: &str, include_internal: bool) -> Result<LocalChangeSummary> {
        let operation = self.resolve_operation(revision).await?;
        self.operation_change(&operation, include_internal).await
    }

    pub async fn working_diff(&mut self, include_internal: bool) -> Result<LocalChangeSummary> {
        let checkpoint = self.checkpoint("inspect local diff").await?;
        if checkpoint.changed {
            self.show("@", include_internal).await
        } else {
            let mut summary = self.show("@", include_internal).await?;
            summary.files.clear();
            summary.patch.clear();
            Ok(summary)
        }
    }

    pub async fn restore(&mut self, revision: &str) -> Result<Checkpoint> {
        let target = self.resolve_operation(revision).await?;
        self.checkpoint("capture local state before restore")
            .await?;
        self.restore_operation(
            &target,
            &format!("jujuleaf restore {}", &target.id().hex()[..12]),
        )
        .await
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

    #[tokio::test]
    async fn history_show_and_restore_follow_operation_history() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = JjWorkspace::init(temp.path()).await.unwrap();
        let file = temp.path().join("main.tex");
        std::fs::write(&file, "one\n").unwrap();
        let first = workspace.checkpoint("one").await.unwrap();
        std::fs::write(&file, "two\n").unwrap();
        workspace.checkpoint("two").await.unwrap();

        let history = workspace.history(10).await.unwrap();
        assert!(history.entries.len() >= 2);
        assert!(history.entries[0].description.contains("two"));
        let shown = workspace.show("@", false).await.unwrap();
        assert_eq!(shown.files[0].path, "main.tex");
        assert!(shown.patch.contains("-one"));
        assert!(shown.patch.contains("+two"));

        let restored = workspace.restore(&first.operation_id).await.unwrap();
        assert!(restored.changed);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
    }

    #[tokio::test]
    async fn checkpoint_honors_jujuleafignore_for_new_files() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = JjWorkspace::init(temp.path()).await.unwrap();
        std::fs::write(temp.path().join(ignore::IGNORE_FILE), "*.aux\n").unwrap();
        std::fs::write(temp.path().join("main.tex"), "paper\n").unwrap();
        std::fs::write(temp.path().join("paper.aux"), "generated\n").unwrap();
        workspace.checkpoint("paper").await.unwrap();

        let shown = workspace.show("@", true).await.unwrap();
        assert!(shown.files.iter().any(|file| file.path == "main.tex"));
        assert!(
            shown
                .files
                .iter()
                .any(|file| file.path == ignore::IGNORE_FILE)
        );
        assert!(!shown.files.iter().any(|file| file.path == "paper.aux"));
    }
}
