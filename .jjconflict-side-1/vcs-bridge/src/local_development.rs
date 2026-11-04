// Copyright 2023 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Explicit development scanner; never the production VFS working-copy type.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use jj_lib::commit::Commit;
use jj_lib::local_working_copy::LocalWorkingCopy;
use jj_lib::merged_tree::MergedTree;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;
use jj_lib::working_copy::CheckoutError;
use jj_lib::working_copy::CheckoutStats;
use jj_lib::working_copy::LockedWorkingCopy;
use jj_lib::working_copy::ResetError;
use jj_lib::working_copy::SnapshotError;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::working_copy::SnapshotStats;
use jj_lib::working_copy::WorkingCopy;
use jj_lib::working_copy::WorkingCopyFactory;
use jj_lib::working_copy::WorkingCopyStateError;

pub const TYPE_NAME: &str = "rudder-local-development-v1";

/// API/compatibility prototype only. Delegating to LocalWorkingCopy still scans
/// source and performs real filesystem checkout. No VFS/live checkpoint claim.
struct LocalDevelopmentWorkingCopy {
    inner: Box<dyn WorkingCopy>,
    state_path: PathBuf,
}

impl LocalDevelopmentWorkingCopy {
    fn type_name() -> &'static str {
        TYPE_NAME
    }

    fn init(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        settings: &UserSettings,
    ) -> Result<Self, WorkingCopyStateError> {
        let inner = LocalWorkingCopy::init(
            store,
            working_copy_path,
            state_path.clone(),
            operation_id,
            workspace_name,
            settings,
        )?;

        Ok(Self {
            inner: Box::new(inner),
            state_path,
        })
    }

    fn load(
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        settings: &UserSettings,
    ) -> Result<Self, WorkingCopyStateError> {
        let inner = LocalWorkingCopy::load(store, working_copy_path, state_path.clone(), settings)?;

        Ok(Self {
            inner: Box::new(inner),
            state_path,
        })
    }
}

impl WorkingCopy for LocalDevelopmentWorkingCopy {
    fn name(&self) -> &str {
        Self::type_name()
    }
    fn workspace_name(&self) -> &WorkspaceName {
        self.inner.workspace_name()
    }
    fn operation_id(&self) -> &OperationId {
        self.inner.operation_id()
    }
    fn tree(&self) -> Result<&MergedTree, WorkingCopyStateError> {
        self.inner.tree()
    }
    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        self.inner.sparse_patterns()
    }
    fn start_mutation(&self) -> Result<Box<dyn LockedWorkingCopy>, WorkingCopyStateError> {
        let inner = self.inner.start_mutation()?;

        Ok(Box::new(LockedLocalDevelopmentWorkingCopy {
            inner,
            state_path: self.state_path.clone(),
        }))
    }
}

pub struct LocalDevelopmentWorkingCopyFactory;

impl WorkingCopyFactory for LocalDevelopmentWorkingCopyFactory {
    fn init_working_copy(
        &self,
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        settings: &UserSettings,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        Ok(Box::new(LocalDevelopmentWorkingCopy::init(
            store,
            working_copy_path,
            state_path,
            operation_id,
            workspace_name,
            settings,
        )?))
    }
    fn load_working_copy(
        &self,
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        settings: &UserSettings,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        Ok(Box::new(LocalDevelopmentWorkingCopy::load(
            store,
            working_copy_path,
            state_path,
            settings,
        )?))
    }
}

struct LockedLocalDevelopmentWorkingCopy {
    inner: Box<dyn LockedWorkingCopy + Send>,
    state_path: PathBuf,
}

#[async_trait]
impl LockedWorkingCopy for LockedLocalDevelopmentWorkingCopy {
    fn old_operation_id(&self) -> &OperationId {
        self.inner.old_operation_id()
    }
    fn old_tree(&self) -> &MergedTree {
        self.inner.old_tree()
    }
    async fn snapshot(
        &mut self,
        options: &SnapshotOptions,
    ) -> Result<(MergedTree, SnapshotStats), SnapshotError> {
        self.inner.snapshot(options).await
    }
    async fn check_out(&mut self, commit: &Commit) -> Result<CheckoutStats, CheckoutError> {
        self.inner.check_out(commit).await
    }
    fn rename_workspace(&mut self, new_name: WorkspaceNameBuf) {
        self.inner.rename_workspace(new_name);
    }
    async fn reset(&mut self, commit: &Commit) -> Result<(), ResetError> {
        self.inner.reset(commit).await
    }
    async fn recover(&mut self, commit: &Commit) -> Result<(), ResetError> {
        self.inner.recover(commit).await
    }
    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        self.inner.sparse_patterns()
    }
    async fn set_sparse_patterns(
        &mut self,
        new_sparse_patterns: Vec<RepoPathBuf>,
    ) -> Result<CheckoutStats, CheckoutError> {
        self.inner.set_sparse_patterns(new_sparse_patterns).await
    }
    async fn finish(
        self: Box<Self>,
        operation_id: OperationId,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let inner = self.inner.finish(operation_id).await?;

        Ok(Box::new(LocalDevelopmentWorkingCopy {
            inner,
            state_path: self.state_path,
        }))
    }
}
