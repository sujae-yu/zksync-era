use std::num::NonZeroU32;

use anyhow::Context as _;
use zksync_basic_types::{vm::FastVmMode, L1BatchNumber};
use zksync_config::configs;
use zksync_protobuf::{repr::ProtoRepr, required};

use crate::{proto::experimental as proto, read_optional_repr};

fn parse_vm_mode(raw: Option<i32>) -> anyhow::Result<FastVmMode> {
    Ok(raw
        .map(proto::FastVmMode::try_from)
        .transpose()
        .context("fast_vm_mode")?
        .map_or_else(FastVmMode::default, |mode| mode.parse()))
}

impl ProtoRepr for proto::Db {
    type Type = configs::ExperimentalDBConfig;

    fn read(&self) -> anyhow::Result<Self::Type> {
        let state_keeper_db_block_cache_capacity_mb =
            required(&self.state_keeper_db_block_cache_capacity_mb)
                .and_then(|&capacity| Ok(capacity.try_into()?))
                .context("state_keeper_db_block_cache_capacity_mb")?;
        Ok(configs::ExperimentalDBConfig {
            state_keeper_db_block_cache_capacity_mb,
            state_keeper_db_max_open_files: self
                .state_keeper_db_max_open_files
                .map(|count| NonZeroU32::new(count).context("cannot be 0"))
                .transpose()
                .context("state_keeper_db_max_open_files")?,
            protective_reads_persistence_enabled: self.reads_persistence_enabled.unwrap_or(false),
            processing_delay_ms: self.processing_delay_ms.unwrap_or_default(),
            include_indices_and_filters_in_block_cache: self
                .include_indices_and_filters_in_block_cache
                .unwrap_or(false),
            merkle_tree_repair_stale_keys: self.merkle_tree_repair_stale_keys.unwrap_or(false),
        })
    }

    fn build(this: &Self::Type) -> Self {
        Self {
            state_keeper_db_block_cache_capacity_mb: Some(
                this.state_keeper_db_block_cache_capacity_mb
                    .try_into()
                    .expect("state_keeper_db_block_cache_capacity_mb"),
            ),
            state_keeper_db_max_open_files: this
                .state_keeper_db_max_open_files
                .map(NonZeroU32::get),
            reads_persistence_enabled: Some(this.protective_reads_persistence_enabled),
            processing_delay_ms: Some(this.processing_delay_ms),
            include_indices_and_filters_in_block_cache: Some(
                this.include_indices_and_filters_in_block_cache,
            ),
            merkle_tree_repair_stale_keys: Some(this.merkle_tree_repair_stale_keys),
        }
    }
}

impl proto::FastVmMode {
    fn new(source: FastVmMode) -> Self {
        match source {
            FastVmMode::Old => Self::Old,
            FastVmMode::New => Self::New,
            FastVmMode::Shadow => Self::Shadow,
        }
    }

    fn parse(&self) -> FastVmMode {
        match self {
            Self::Old => FastVmMode::Old,
            Self::New => FastVmMode::New,
            Self::Shadow => FastVmMode::Shadow,
        }
    }
}

impl ProtoRepr for proto::VmPlayground {
    type Type = configs::ExperimentalVmPlaygroundConfig;

    fn read(&self) -> anyhow::Result<Self::Type> {
        Ok(Self::Type {
            fast_vm_mode: self
                .fast_vm_mode
                .map(proto::FastVmMode::try_from)
                .transpose()
                .context("fast_vm_mode")?
                .map_or_else(FastVmMode::default, |mode| mode.parse()),
            db_path: self.db_path.clone(),
            first_processed_batch: L1BatchNumber(self.first_processed_batch.unwrap_or(0)),
            window_size: NonZeroU32::new(self.window_size.unwrap_or(1))
                .context("window_size cannot be 0")?,
            reset: self.reset.unwrap_or(false),
        })
    }

    fn build(this: &Self::Type) -> Self {
        Self {
            fast_vm_mode: Some(proto::FastVmMode::new(this.fast_vm_mode).into()),
            db_path: this.db_path.clone(),
            first_processed_batch: Some(this.first_processed_batch.0),
            window_size: Some(this.window_size.get()),
            reset: Some(this.reset),
        }
    }
}

impl ProtoRepr for proto::Vm {
    type Type = configs::ExperimentalVmConfig;

    fn read(&self) -> anyhow::Result<Self::Type> {
        Ok(Self::Type {
            playground: read_optional_repr(&self.playground).unwrap_or_default(),
            state_keeper_fast_vm_mode: parse_vm_mode(self.state_keeper_fast_vm_mode)?,
            api_fast_vm_mode: parse_vm_mode(self.api_fast_vm_mode)?,
        })
    }

    fn build(this: &Self::Type) -> Self {
        Self {
            playground: Some(ProtoRepr::build(&this.playground)),
            state_keeper_fast_vm_mode: Some(
                proto::FastVmMode::new(this.state_keeper_fast_vm_mode).into(),
            ),
            api_fast_vm_mode: Some(proto::FastVmMode::new(this.api_fast_vm_mode).into()),
        }
    }
}
