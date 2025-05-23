// Copyright (c) 2023 - 2025 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use codederror::CodedError;

use restate_bifrost::Bifrost;
use restate_core::network::MessageRouterBuilder;
use restate_core::network::Networking;
use restate_core::network::TransportConnect;
use restate_core::partitions::PartitionRouting;
use restate_core::worker_api::ProcessorsManagerHandle;
use restate_core::{Metadata, MetadataKind, cancellation_watcher};
use restate_core::{MetadataWriter, TaskCenter};
use restate_core::{ShutdownError, TaskKind};
use restate_storage_query_datafusion::context::QueryContext;
use restate_types::Version;
use restate_types::config::Configuration;
use restate_types::health::HealthStatus;
use restate_types::live::Live;
use restate_types::partitions::state::PartitionReplicaSetStates;
use restate_types::protobuf::common::WorkerStatus;
use restate_types::schema::subscriptions::SubscriptionResolver;
use restate_worker::SubscriptionController;
use restate_worker::Worker;

#[derive(Debug, thiserror::Error, CodedError)]
pub enum WorkerRoleError {
    #[error("worker failed: {0}")]
    Worker(
        #[from]
        #[code]
        restate_worker::Error,
    ),
    #[error(transparent)]
    #[code(unknown)]
    Shutdown(#[from] ShutdownError),
}

#[derive(Debug, thiserror::Error, CodedError)]
pub enum SchemaError {
    #[error("failed to fetch schema updates: {0}")]
    #[code(unknown)]
    Fetch(#[from] tonic::Status),
    #[error("failed updating subscriptions: {0}")]
    #[code(unknown)]
    Subscription(#[from] restate_worker::WorkerHandleError),
}

#[derive(Debug, thiserror::Error, CodedError)]
pub enum WorkerRoleBuildError {
    #[error("failed creating worker: {0}")]
    Worker(
        #[from]
        #[code]
        restate_worker::BuildError,
    ),
}

pub struct WorkerRole {
    metadata: Metadata,
    worker: Worker,
}

impl WorkerRole {
    #[allow(clippy::too_many_arguments)]
    pub async fn create<T: TransportConnect>(
        health_status: HealthStatus<WorkerStatus>,
        metadata: Metadata,
        partition_routing: PartitionRouting,
        replica_set_states: PartitionReplicaSetStates,
        updateable_config: Live<Configuration>,
        router_builder: &mut MessageRouterBuilder,
        networking: Networking<T>,
        bifrost: Bifrost,
        metadata_writer: MetadataWriter,
    ) -> Result<Self, WorkerRoleBuildError> {
        let worker = Worker::create(
            updateable_config,
            health_status,
            metadata.clone(),
            partition_routing,
            replica_set_states,
            networking,
            bifrost,
            router_builder,
            metadata_writer,
        )
        .await?;

        Ok(WorkerRole { worker, metadata })
    }

    pub fn partition_processor_manager_handle(&self) -> ProcessorsManagerHandle {
        self.worker.partition_processor_manager_handle()
    }

    pub fn storage_query_context(&self) -> &QueryContext {
        self.worker.storage_query_context()
    }

    pub async fn start(self) -> anyhow::Result<()> {
        // todo: only run subscriptions on node 0 once being distributed
        TaskCenter::spawn_child(
            TaskKind::MetadataBackgroundSync,
            "subscription_controller",
            Self::watch_subscriptions(self.metadata, self.worker.subscription_controller_handle()),
        )?;

        TaskCenter::spawn_child(TaskKind::RoleRunner, "worker-service", async {
            self.worker.run().await
        })?;

        Ok(())
    }

    async fn watch_subscriptions<SC>(
        metadata: Metadata,
        subscription_controller: SC,
    ) -> anyhow::Result<()>
    where
        SC: SubscriptionController + Clone + Send + Sync,
    {
        let schema_view = metadata.updateable_schema();
        let mut next_version = Version::MIN;
        let cancellation_watcher = cancellation_watcher();
        tokio::pin!(cancellation_watcher);

        loop {
            tokio::select! {
                _ = &mut cancellation_watcher => {
                    break;
                },
                version = metadata.wait_for_version(MetadataKind::Schema, next_version) => {
                    next_version = version?.next();

                    // This might return subscriptions belonging to a higher schema version. As a
                    // result we might re-apply the same list of subscriptions. This is not a
                    // problem, since update_subscriptions is idempotent.
                    let subscriptions = schema_view.pinned().list_subscriptions(&[]);
                    subscription_controller
                        .update_subscriptions(subscriptions)
                        .await?;
                }
            }
        }

        Ok(())
    }
}
