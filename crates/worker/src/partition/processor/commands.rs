// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// todo: remove when using this module
#![allow(dead_code)]

use restate_bifrost::DataRecord;
use restate_types::logs::Lsn;
use restate_wal_protocol::v2::{self, Envelope};

use crate::partition::ProcessorError;

#[derive(Debug)]
pub enum NextStep {
    AdvanceLastAppliedLsn(Lsn, v2::Dedup),
}

/// Applies a single partition-scoped record to the processor.
pub trait ApplyPartitionCommand<M> {
    async fn apply(&mut self, record: DataRecord<Envelope<M>>) -> Result<NextStep, ProcessorError>;
}
