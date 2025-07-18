use std::{convert::TryFrom, num::NonZeroU64, str::FromStr};

use anyhow::Context as _;
use zksync_db_connection::{
    connection::Connection, error::DalResult, instrument::InstrumentExt, interpolate_query,
    match_query_as,
};
use zksync_types::{
    aggregated_operations::{
        AggregatedActionType, L1BatchAggregatedActionType, L2BlockAggregatedActionType,
    },
    eth_sender::{EthTx, EthTxBlobSidecar, EthTxFinalityStatus, TxHistory},
    Address, L1BatchNumber, SLChainId, H256, U256,
};

use crate::{
    models::storage_eth_tx::{BlocksEthSenderStats, StorageEthTx, StorageTxHistory},
    Core,
};

#[derive(Debug)]
pub struct EthSenderDal<'a, 'c> {
    pub(crate) storage: &'a mut Connection<'c, Core>,
}

impl EthSenderDal<'_, '_> {
    pub async fn get_non_final_txs(
        &mut self,
        operator_address: Address,
        is_gateway: bool,
    ) -> sqlx::Result<Vec<EthTx>> {
        let txs = sqlx::query_as!(
            StorageEthTx,
            r#"
            SELECT
                eth_txs.*
            FROM
                eth_txs
            JOIN eth_txs_history ON eth_txs.confirmed_eth_tx_history_id = eth_txs_history.id
            WHERE
                from_addr = $1
                AND is_gateway = $2
                AND eth_txs_history.finality_status != 'finalized'
            ORDER BY
                eth_txs.id
            "#,
            operator_address.as_bytes(),
            is_gateway,
        )
        .fetch_all(self.storage.conn())
        .await?;
        Ok(txs.into_iter().map(|tx| tx.into()).collect())
    }

    pub async fn unfinalize_txs(
        &mut self,
        operator_address: Address,
        is_gateway: bool,
        from_eth_tx_id: u32,
    ) -> anyhow::Result<()> {
        let mut transaction = self
            .storage
            .start_transaction()
            .await
            .context("start_transaction()")?;
        sqlx::query!(
            r#"
            UPDATE eth_txs
            SET
                confirmed_eth_tx_history_id = NULL,
                gas_used = NULL,
                has_failed = FALSE
            WHERE
                id >= $1
                AND from_addr = $2
                AND is_gateway = $3
            "#,
            from_eth_tx_id as i32,
            operator_address.as_bytes(),
            is_gateway,
        )
        .execute(transaction.conn())
        .await?;
        sqlx::query!(
            r#"
            UPDATE eth_txs_history
            SET
                confirmed_at = NULL,
                finality_status = 'pending',
                sent_successfully = FALSE
            WHERE
                eth_tx_id >= $1
                AND sent_successfully = TRUE
            "#,
            from_eth_tx_id as i32,
        )
        .execute(transaction.conn())
        .await?;
        transaction.commit().await.context("commit_transaction()")?;
        Ok(())
    }

    pub async fn get_inflight_txs(
        &mut self,
        operator_address: Address,
        is_gateway: bool,
    ) -> sqlx::Result<Vec<EthTx>> {
        let txs = sqlx::query_as!(
            StorageEthTx,
            r#"
            SELECT
                *
            FROM
                eth_txs
            WHERE
                from_addr = $1
                AND is_gateway = $2
                AND confirmed_eth_tx_history_id IS NULL
                AND id <= COALESCE(
                    (SELECT
                        eth_tx_id
                    FROM
                        eth_txs_history
                    JOIN eth_txs ON eth_txs.id = eth_txs_history.eth_tx_id
                    WHERE
                        eth_txs_history.finality_status != 'finalized'
                        AND
                        from_addr = $1
                        AND is_gateway = $2
                    ORDER BY eth_tx_id DESC LIMIT 1),
                    0
                )
            ORDER BY
                id
            "#,
            operator_address.as_bytes(),
            is_gateway,
        )
        .fetch_all(self.storage.conn())
        .await?;
        Ok(txs.into_iter().map(|tx| tx.into()).collect())
    }

    pub async fn get_inflight_txs_count_for_gateway_migration(
        &mut self,
        is_gateway: bool,
    ) -> sqlx::Result<usize> {
        let count = sqlx::query!(
            r#"
            SELECT
                COUNT(*)
            FROM
                eth_txs
            WHERE
                confirmed_eth_tx_history_id IS NULL
                AND is_gateway = $1
            "#,
            is_gateway
        )
        .fetch_one(self.storage.conn())
        .await?
        .count
        .unwrap();
        Ok(count.try_into().unwrap())
    }

    pub async fn get_chain_id_of_oldest_unfinalized_eth_tx(&mut self) -> DalResult<Option<u64>> {
        let res = sqlx::query!(
            r#"
            SELECT
                chain_id
            FROM
                eth_txs
            INNER JOIN l1_batches
                ON
                    eth_txs.id = l1_batches.eth_commit_tx_id
                    OR eth_txs.id = l1_batches.eth_prove_tx_id
                    OR eth_txs.id = l1_batches.eth_execute_tx_id
            LEFT JOIN eth_txs_history ON eth_txs.id = eth_txs_history.eth_tx_id
            WHERE
                eth_txs_history.finality_status != 'finalized'
            ORDER BY l1_batches.number ASC
            LIMIT 1
            "#,
        )
        .instrument("get_chain_id_of_oldest_unfinalized_eth_tx")
        .fetch_optional(self.storage)
        .await?
        .and_then(|row| row.chain_id.map(|a| a as u64));
        Ok(res)
    }

    pub async fn get_unconfirmed_txs_count(&mut self) -> DalResult<usize> {
        let count = sqlx::query!(
            r#"
            SELECT
                COUNT(*)
            FROM
                eth_txs
            WHERE
                confirmed_eth_tx_history_id IS NULL
            "#
        )
        .instrument("get_unconfirmed_txs_count")
        .fetch_one(self.storage)
        .await?
        .count
        .unwrap();
        Ok(count.try_into().unwrap())
    }

    pub async fn get_eth_all_blocks_stat(&mut self) -> sqlx::Result<BlocksEthSenderStats> {
        struct EthTxRow {
            number: i64,
            confirmed: bool,
        }

        const TX_TYPES: &[AggregatedActionType] = &[
            AggregatedActionType::L1Batch(L1BatchAggregatedActionType::Commit),
            AggregatedActionType::L1Batch(L1BatchAggregatedActionType::PublishProofOnchain),
            AggregatedActionType::L1Batch(L1BatchAggregatedActionType::Execute),
            AggregatedActionType::L2Block(L2BlockAggregatedActionType::Precommit),
        ];

        let mut stats = BlocksEthSenderStats::default();
        for &tx_type in TX_TYPES {
            let mut tx_rows = vec![];
            for confirmed in [true, false] {
                let query = match_query_as!(
                    EthTxRow,
                    [
                        "SELECT number AS number, ", _, " AS \"confirmed!\" FROM ",_ ,
                        " INNER JOIN eth_txs_history ON ", _, " = eth_txs_history.eth_tx_id ",
                        _, // WHERE clause
                        " ORDER BY number DESC LIMIT 1"
                    ],
                    match ((confirmed, tx_type)) {
                        (false, AggregatedActionType::L1Batch( L1BatchAggregatedActionType::Commit)) => ("false", "l1_batches", "l1_batches.eth_commit_tx_id", "";),
                        (true, AggregatedActionType::L1Batch( L1BatchAggregatedActionType::Commit)) => (
                            "true", "l1_batches", "l1_batches.eth_commit_tx_id", "WHERE eth_txs_history.confirmed_at IS NOT NULL";
                        ),
                        (false,AggregatedActionType::L1Batch(L1BatchAggregatedActionType::PublishProofOnchain)) => ("false", "l1_batches", "l1_batches.eth_prove_tx_id", "";),
                        (true, AggregatedActionType::L1Batch(L1BatchAggregatedActionType::PublishProofOnchain)) => (
                            "true", "l1_batches", "l1_batches.eth_prove_tx_id", "WHERE eth_txs_history.confirmed_at IS NOT NULL";
                        ),
                        (false,AggregatedActionType::L1Batch( L1BatchAggregatedActionType::Execute)) => ("false", "l1_batches", "l1_batches.eth_execute_tx_id", "";),
                        (true,AggregatedActionType::L1Batch( L1BatchAggregatedActionType::Execute)) => (
                            "true", "l1_batches", "l1_batches.eth_execute_tx_id", "WHERE eth_txs_history.confirmed_at IS NOT NULL";
                        ),
                        (false,AggregatedActionType::L2Block(L2BlockAggregatedActionType::Precommit)) => ("false", "miniblocks", "miniblocks.eth_precommit_tx_id", "";),
                        (true,AggregatedActionType::L2Block(L2BlockAggregatedActionType::Precommit)) => (
                            "true", "miniblocks", "miniblocks.eth_precommit_tx_id", "WHERE eth_txs_history.confirmed_at IS NOT NULL";
                        ),
                    }
                );
                tx_rows.extend(query.fetch_all(self.storage.conn()).await?);
            }

            for row in tx_rows {
                let block_number = row.number as u32;
                if row.confirmed {
                    stats.mined.push((tx_type, block_number));
                } else {
                    stats.saved.push((tx_type, block_number));
                }
            }
        }
        Ok(stats)
    }

    pub async fn get_eth_tx(&mut self, eth_tx_id: u32) -> sqlx::Result<Option<EthTx>> {
        Ok(sqlx::query_as!(
            StorageEthTx,
            r#"
            SELECT
                *
            FROM
                eth_txs
            WHERE
                id = $1
            "#,
            eth_tx_id as i32
        )
        .fetch_optional(self.storage.conn())
        .await?
        .map(Into::into))
    }

    pub async fn get_new_eth_txs(
        &mut self,
        limit: u64,
        operator_address: Address,
        is_gateway: bool,
    ) -> sqlx::Result<Vec<EthTx>> {
        let txs = sqlx::query_as!(
            StorageEthTx,
            r#"
            SELECT
                *
            FROM
                eth_txs
            WHERE
                from_addr = $2
                AND is_gateway = $3
                AND id > COALESCE(
                    (SELECT
                        eth_tx_id
                    FROM
                        eth_txs_history
                    JOIN eth_txs ON eth_txs.id = eth_txs_history.eth_tx_id
                    WHERE
                        eth_txs_history.sent_at_block IS NOT NULL
                        AND from_addr = $2
                        AND is_gateway = $3
                        AND sent_successfully = TRUE
                    ORDER BY eth_tx_id DESC LIMIT 1),
                    0
                )
            ORDER BY
                id
            LIMIT
                $1
            "#,
            limit as i64,
            operator_address.as_bytes(),
            is_gateway
        )
        .fetch_all(self.storage.conn())
        .await?;
        Ok(txs.into_iter().map(|tx| tx.into()).collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn save_eth_tx(
        &mut self,
        nonce: u64,
        raw_tx: Vec<u8>,
        tx_type: AggregatedActionType,
        contract_address: Address,
        predicted_gas_cost: Option<u64>,
        from_address: Option<Address>,
        blob_sidecar: Option<EthTxBlobSidecar>,
        is_gateway: bool,
    ) -> sqlx::Result<EthTx> {
        let address = format!("{:#x}", contract_address);
        let eth_tx = sqlx::query_as!(
            StorageEthTx,
            r#"
            INSERT INTO
            eth_txs (
                raw_tx,
                nonce,
                tx_type,
                contract_address,
                predicted_gas_cost,
                created_at,
                updated_at,
                from_addr,
                blob_sidecar,
                is_gateway
            )
            VALUES
            ($1, $2, $3, $4, $5, NOW(), NOW(), $6, $7, $8)
            RETURNING
            *
            "#,
            raw_tx,
            nonce as i64,
            tx_type.to_string(),
            address,
            predicted_gas_cost.map(|c| c as i64),
            from_address.as_ref().map(Address::as_bytes),
            blob_sidecar.map(|sidecar| bincode::serialize(&sidecar)
                .expect("can always bincode serialize EthTxBlobSidecar; qed")),
            is_gateway,
        )
        .fetch_one(self.storage.conn())
        .await?;
        Ok(eth_tx.into())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_tx_history(
        &mut self,
        eth_tx_id: u32,
        base_fee_per_gas: u64,
        priority_fee_per_gas: u64,
        blob_base_fee_per_gas: Option<u64>,
        max_gas_per_pubdata: Option<u64>,
        tx_hash: H256,
        raw_signed_tx: &[u8],
        sent_at_block: u32,
        predicted_gas_limit: Option<u64>,
    ) -> anyhow::Result<Option<u32>> {
        let priority_fee_per_gas =
            i64::try_from(priority_fee_per_gas).context("Can't convert u64 to i64")?;
        let base_fee_per_gas =
            i64::try_from(base_fee_per_gas).context("Can't convert u64 to i64")?;
        let tx_hash = format!("{:#x}", tx_hash);

        Ok(sqlx::query!(
            r#"
            INSERT INTO
            eth_txs_history (
                eth_tx_id,
                base_fee_per_gas,
                priority_fee_per_gas,
                tx_hash,
                signed_raw_tx,
                created_at,
                updated_at,
                blob_base_fee_per_gas,
                max_gas_per_pubdata,
                predicted_gas_limit,
                sent_at_block,
                sent_at,
                sent_successfully,
                finality_status
            
            )
            VALUES
            ($1, $2, $3, $4, $5, NOW(), NOW(), $6, $7, $8, $9, NOW(), FALSE, 'pending')
            ON CONFLICT (tx_hash) DO UPDATE SET sent_at_block = $9
            RETURNING
            id
            "#,
            eth_tx_id as i32,
            base_fee_per_gas,
            priority_fee_per_gas,
            tx_hash,
            raw_signed_tx,
            blob_base_fee_per_gas.map(|v| v as i64),
            max_gas_per_pubdata.map(|v| v as i64),
            predicted_gas_limit.map(|v| v as i64),
            sent_at_block as i32
        )
        .fetch_optional(self.storage.conn())
        .await?
        .map(|row| row.id as u32))
    }

    pub async fn tx_history_by_hash(
        &mut self,
        eth_tx_id: u32,
        tx_hash: H256,
    ) -> anyhow::Result<Option<u32>> {
        let tx_hash = format!("{:#x}", tx_hash);
        Ok(sqlx::query!(
            r#"
            SELECT id FROM eth_txs_history
            WHERE eth_tx_id = $1 AND tx_hash = $2
            "#,
            eth_tx_id as i32,
            tx_hash,
        )
        .fetch_optional(self.storage.conn())
        .await?
        .map(|row| row.id as u32))
    }

    pub async fn set_sent_success(&mut self, eth_txs_history_id: u32) -> sqlx::Result<()> {
        sqlx::query!(
            r#"
            UPDATE eth_txs_history
            SET
                sent_successfully = TRUE,
                updated_at = NOW()
            WHERE
                id = $1
            "#,
            eth_txs_history_id as i32
        )
        .execute(self.storage.conn())
        .await?;
        Ok(())
    }

    pub async fn confirm_tx(
        &mut self,
        tx_hash: H256,
        eth_tx_finality_status: EthTxFinalityStatus,
        gas_used: U256,
    ) -> anyhow::Result<()> {
        let mut transaction = self
            .storage
            .start_transaction()
            .await
            .context("start_transaction()")?;
        let gas_used = i64::try_from(gas_used)
            .map_err(|err| anyhow::anyhow!("Can't convert U256 to i64: {err}"))?;
        let tx_hash = format!("{:#x}", tx_hash);
        let ids = sqlx::query!(
            r#"
            UPDATE eth_txs_history
            SET
                updated_at = NOW(),
                confirmed_at = NOW(),
                finality_status = $2,
                sent_successfully = TRUE
            WHERE
                tx_hash = $1
            RETURNING
            id,
            eth_tx_id
            "#,
            tx_hash,
            eth_tx_finality_status.to_string()
        )
        .fetch_one(transaction.conn())
        .await?;

        sqlx::query!(
            r#"
            UPDATE eth_txs
            SET
                gas_used = $1,
                confirmed_eth_tx_history_id = $2
            WHERE
                id = $3
            "#,
            gas_used,
            ids.id,
            ids.eth_tx_id
        )
        .execute(transaction.conn())
        .await?;

        transaction.commit().await?;
        Ok(())
    }

    pub async fn set_chain_id(&mut self, eth_tx_id: u32, chain_id: u64) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
            UPDATE eth_txs
            SET
                chain_id = $1
            WHERE
                id = $2
            "#,
            chain_id as i64,
            eth_tx_id as i32,
        )
        .execute(self.storage.conn())
        .await?;
        Ok(())
    }

    pub async fn get_batch_commit_chain_id(
        &mut self,
        batch_number: L1BatchNumber,
    ) -> DalResult<Option<SLChainId>> {
        let row = sqlx::query!(
            r#"
            SELECT eth_txs.chain_id
            FROM l1_batches
            JOIN eth_txs ON eth_txs.id = l1_batches.eth_commit_tx_id
            WHERE
                number = $1
            "#,
            i64::from(batch_number.0),
        )
        .instrument("get_batch_commit_chain_id")
        .with_arg("batch_number", &batch_number)
        .fetch_optional(self.storage)
        .await?;
        Ok(row.and_then(|r| r.chain_id).map(|id| SLChainId(id as u64)))
    }

    pub async fn get_batch_execute_chain_id(
        &mut self,
        batch_number: L1BatchNumber,
    ) -> DalResult<Option<SLChainId>> {
        let row = sqlx::query!(
            r#"
            SELECT eth_txs.chain_id
            FROM l1_batches
            JOIN eth_txs ON eth_txs.id = l1_batches.eth_execute_tx_id
            WHERE
                number = $1
            "#,
            i64::from(batch_number.0),
        )
        .instrument("get_batch_execute_chain_id")
        .with_arg("batch_number", &batch_number)
        .fetch_optional(self.storage)
        .await?;
        Ok(row.and_then(|r| r.chain_id).map(|id| SLChainId(id as u64)))
    }

    pub async fn get_confirmed_tx_hash_by_eth_tx_id(
        &mut self,
        eth_tx_id: u32,
    ) -> anyhow::Result<Option<H256>> {
        let tx_hash = sqlx::query!(
            r#"
            SELECT
                tx_hash
            FROM
                eth_txs_history
            WHERE
                eth_tx_id = $1
                AND confirmed_at IS NOT NULL
            "#,
            eth_tx_id as i32
        )
        .fetch_optional(self.storage.conn())
        .await?;

        let Some(tx_hash) = tx_hash else {
            return Ok(None);
        };
        let tx_hash = tx_hash.tx_hash;
        let tx_hash = tx_hash.trim_start_matches("0x");
        Ok(Some(H256::from_str(tx_hash).context("invalid tx_hash")?))
    }

    /// This method inserts a pending transaction into eth_txs_history table.
    /// It should be used only in external node context as most properties are not set.
    /// Inserted transaction does not need to be validated as its set as pending.
    /// Validation needs to be done before marking with one of the executed statuses.
    pub async fn insert_pending_received_eth_tx(
        &mut self,
        l1_batch: L1BatchNumber,
        tx_type: L1BatchAggregatedActionType,
        tx_hash: H256,
        sl_chain_id: Option<SLChainId>,
    ) -> anyhow::Result<()> {
        let mut transaction = self
            .storage
            .start_transaction()
            .await
            .context("start_transaction")?;
        let tx_hash = format!("{:#x}", tx_hash);

        let eth_tx_id = sqlx::query_scalar!(
            "SELECT eth_txs.id FROM eth_txs_history JOIN eth_txs \
            ON eth_txs.id = eth_txs_history.eth_tx_id \
            WHERE eth_txs_history.tx_hash = $1",
            tx_hash
        )
        .fetch_optional(transaction.conn())
        .await?;

        // Check if the transaction with the corresponding hash already exists.
        let eth_tx_id = if let Some(eth_tx_id) = eth_tx_id {
            eth_tx_id
        } else {
            // No such transaction in the database yet, we have to insert it.

            // Insert general tx descriptor.
            let eth_tx_id = sqlx::query_scalar!(
                "INSERT INTO eth_txs (raw_tx, nonce, tx_type, contract_address, predicted_gas_cost, chain_id, created_at, updated_at) \
                VALUES ('\\x00', 0, $1, '', NULL, $2, now(), now()) \
                RETURNING id",
                tx_type.to_string(),
                sl_chain_id.map(|chain_id| chain_id.0 as i64)
            )
            .fetch_one(transaction.conn())
            .await?;

            // Insert a "sent transaction".
            sqlx::query_scalar!(
                "INSERT INTO eth_txs_history \
                (eth_tx_id, base_fee_per_gas, priority_fee_per_gas, tx_hash, signed_raw_tx, created_at, updated_at, confirmed_at, sent_successfully, finality_status) \
                VALUES ($1, 0, 0, $2, '\\x00', now(), now(), NULL, TRUE, $3) \
                RETURNING id",
                eth_tx_id,
                tx_hash,
                EthTxFinalityStatus::Pending.to_string()
            )
            .fetch_one(transaction.conn())
            .await?;
            eth_tx_id
        };

        // Tie the ETH tx to the L1 batch.
        super::BlocksDal {
            storage: &mut transaction,
        }
        .set_eth_tx_id_for_l1_batches(
            l1_batch..=l1_batch,
            eth_tx_id as u32,
            AggregatedActionType::L1Batch(tx_type),
        )
        .await
        .context("set_eth_tx_id()")?;

        transaction.commit().await.context("commit()")
    }

    pub async fn get_unfinalized_transactions(
        &mut self,
        limit: NonZeroU64,
        chain_id: Option<SLChainId>,
    ) -> DalResult<Vec<TxHistory>> {
        let limit = i64::try_from(limit.get()).expect("limit overflow");
        let tx_history = match_query_as!(
            StorageTxHistory,
            [r#"
            SELECT
                eth_txs_history.*,
                eth_txs.blob_sidecar,
                eth_txs.tx_type,
                eth_txs.chain_id
            FROM
                eth_txs_history
            LEFT JOIN eth_txs ON eth_tx_id = eth_txs.id
            WHERE
                eth_txs_history.finality_status != 'finalized'
            "#, _,
            r#"
            ORDER BY
                eth_txs_history.id ASC
            LIMIT
                $1
            "#],
            match (chain_id) {
                Some(chain_id) => ("AND eth_txs.chain_id = $2"; limit, chain_id.0 as i64),
                None => (""; limit),
            }
        )
        .instrument("get_unfinalized_transactions")
        .with_arg("chain_id", &chain_id)
        .fetch_all(self.storage)
        .await?;
        Ok(tx_history.into_iter().map(|tx| tx.into()).collect())
    }

    /// Sets `sent_at_block` for the eth_txs_history row.
    /// Used for ExternalNode when syncing batch transaction state from SL.
    pub async fn set_sent_at_block(
        &mut self,
        tx_history_id: u32,
        block_number: u32,
    ) -> DalResult<()> {
        sqlx::query!(
            r#"
            UPDATE eth_txs_history
            SET sent_at_block = $2
            WHERE id = $1
            "#,
            tx_history_id as i32,
            block_number as i32,
        )
        .instrument("set_sent_at_block")
        .with_arg("tx_history_id", &tx_history_id)
        .with_arg("block_number", &block_number)
        .execute(self.storage)
        .await?;
        Ok(())
    }

    /// Sets sent_at_block to null in eth_txs_history row.
    /// Used for ExternalNode when a previously included transaction on SL is excluded due to fork
    pub async fn unset_sent_at_block(&mut self, tx_history_id: u32) -> DalResult<()> {
        sqlx::query!(
            r#"
            UPDATE eth_txs_history
            SET sent_at_block = NULL
            WHERE id = $1
            "#,
            tx_history_id as i32,
        )
        .instrument("unset_sent_at_block")
        .with_arg("tx_history_id", &tx_history_id)
        .execute(self.storage)
        .await?;
        Ok(())
    }

    pub async fn get_eth_tx_history_by_id(
        &mut self,
        eth_tx_history_id: u32,
    ) -> DalResult<TxHistory> {
        let tx_history = sqlx::query_as!(
            StorageTxHistory,
            r#"
            SELECT
                eth_txs_history.*,
                eth_txs.blob_sidecar,
                eth_txs.tx_type,
                eth_txs.chain_id
            FROM
                eth_txs_history
            LEFT JOIN eth_txs ON eth_tx_id = eth_txs.id
            WHERE
                eth_txs_history.id = $1
            "#,
            eth_tx_history_id as i32,
        )
        .instrument("get_eth_tx_history_by_id")
        .with_arg("eth_tx_history_id", &eth_tx_history_id)
        .fetch_one(self.storage)
        .await?;
        Ok(tx_history.into())
    }

    pub async fn get_tx_history_to_check(
        &mut self,
        eth_tx_id: u32,
    ) -> sqlx::Result<Vec<TxHistory>> {
        let tx_history = sqlx::query_as!(
            StorageTxHistory,
            r#"
            SELECT
                eth_txs_history.*,
                eth_txs.blob_sidecar,
                eth_txs.tx_type,
                eth_txs.chain_id
            FROM
                eth_txs_history
            LEFT JOIN eth_txs ON eth_tx_id = eth_txs.id
            WHERE
                eth_tx_id = $1
            ORDER BY
                eth_txs_history.created_at DESC
            "#,
            eth_tx_id as i32
        )
        .fetch_all(self.storage.conn())
        .await?;
        Ok(tx_history.into_iter().map(|tx| tx.into()).collect())
    }

    pub async fn get_block_number_on_first_sent_attempt(
        &mut self,
        eth_tx_id: u32,
    ) -> sqlx::Result<Option<u32>> {
        let sent_at_block = sqlx::query_scalar!(
            "SELECT MIN(sent_at_block) FROM eth_txs_history WHERE eth_tx_id = $1",
            eth_tx_id as i32
        )
        .fetch_optional(self.storage.conn())
        .await?;
        Ok(sent_at_block.flatten().map(|block| block as u32))
    }

    pub async fn get_block_number_on_last_sent_attempt(
        &mut self,
        eth_tx_id: u32,
    ) -> sqlx::Result<Option<u32>> {
        let sent_at_block = sqlx::query_scalar!(
            "SELECT MAX(sent_at_block) FROM eth_txs_history WHERE eth_tx_id = $1",
            eth_tx_id as i32
        )
        .fetch_optional(self.storage.conn())
        .await?;
        Ok(sent_at_block.flatten().map(|block| block as u32))
    }

    pub async fn get_last_sent_successfully_eth_tx(
        &mut self,
        eth_tx_id: u32,
    ) -> sqlx::Result<Option<TxHistory>> {
        let history_item = sqlx::query_as!(
            StorageTxHistory,
            r#"
            SELECT
                eth_txs_history.*,
                eth_txs.blob_sidecar,
                eth_txs.tx_type,
                eth_txs.chain_id
            FROM
                eth_txs_history
            LEFT JOIN eth_txs ON eth_tx_id = eth_txs.id
            WHERE
                eth_tx_id = $1 AND sent_successfully = TRUE
            ORDER BY
                eth_txs_history.created_at DESC
            LIMIT
                1
            "#,
            eth_tx_id as i32
        )
        .fetch_optional(self.storage.conn())
        .await?;
        Ok(history_item.map(|tx| tx.into()))
    }

    pub async fn get_eth_tx_id_by_batch_and_op(
        &mut self,
        l1_batch_number: L1BatchNumber,
        op_type: L1BatchAggregatedActionType,
    ) -> sqlx::Result<Option<u32>> {
        let row = sqlx::query!(
            r#"
            SELECT
                eth_commit_tx_id,
                eth_prove_tx_id,
                eth_execute_tx_id
            FROM
                l1_batches
            WHERE
                number = $1
            "#,
            i64::from(l1_batch_number.0)
        )
        .fetch_optional(self.storage.conn())
        .await?;

        Ok(row.and_then(|row| {
            let tx_id_opt: Option<i32> = match op_type {
                L1BatchAggregatedActionType::Commit => row.eth_commit_tx_id,
                L1BatchAggregatedActionType::PublishProofOnchain => row.eth_prove_tx_id,
                L1BatchAggregatedActionType::Execute => row.eth_execute_tx_id,
            };
            tx_id_opt.map(|id| id as u32)
        }))
    }

    /// Returns the next nonce for the operator account
    pub async fn get_next_nonce(
        &mut self,
        from_address: Address,
        is_gateway: bool,
    ) -> sqlx::Result<Option<u64>> {
        // First query nonce where `from_addr` is set.
        let row = sqlx::query!(
            r#"
            SELECT
                nonce
            FROM
                eth_txs
            WHERE
                from_addr = $1
                AND is_gateway = $2
            ORDER BY
                id DESC
            LIMIT
                1
            "#,
            from_address.as_bytes(),
            is_gateway,
        )
        .fetch_optional(self.storage.conn())
        .await?;

        Ok(row.map(|a| a.nonce as u64 + 1))
    }

    pub async fn mark_failed_transaction(&mut self, eth_tx_id: u32) -> sqlx::Result<()> {
        sqlx::query!(
            r#"
            UPDATE eth_txs
            SET
                has_failed = TRUE
            WHERE
                id = $1
            "#,
            eth_tx_id as i32
        )
        .execute(self.storage.conn())
        .await?;
        Ok(())
    }

    pub async fn get_number_of_failed_transactions(&mut self) -> anyhow::Result<u64> {
        sqlx::query!(
            r#"
            SELECT
                COUNT(*)
            FROM
                eth_txs
            WHERE
                has_failed = TRUE
            "#
        )
        .fetch_one(self.storage.conn())
        .await?
        .count
        .map(|c| c as u64)
        .context("count field is missing")
    }

    pub async fn clear_failed_transactions(&mut self) -> sqlx::Result<()> {
        sqlx::query!(
            r#"
            DELETE FROM eth_txs
            WHERE
                id >= (
                    SELECT
                        MIN(id)
                    FROM
                        eth_txs
                    WHERE
                        has_failed = TRUE
                )
            "#
        )
        .execute(self.storage.conn())
        .await?;
        Ok(())
    }

    pub async fn delete_eth_txs(&mut self, last_batch_to_keep: L1BatchNumber) -> sqlx::Result<()> {
        sqlx::query!(
            r#"
            DELETE FROM eth_txs
            WHERE
                id IN (
                    (
                        SELECT
                            eth_commit_tx_id
                        FROM
                            l1_batches
                        WHERE
                            number > $1
                    )
                    UNION
                    (
                        SELECT
                            eth_prove_tx_id
                        FROM
                            l1_batches
                        WHERE
                            number > $1
                    )
                    UNION
                    (
                        SELECT
                            eth_execute_tx_id
                        FROM
                            l1_batches
                        WHERE
                            number > $1
                    )
                )
            "#,
            i64::from(last_batch_to_keep.0)
        )
        .execute(self.storage.conn())
        .await?;

        Ok(())
    }
}

/// These methods should only be used for tests.
impl EthSenderDal<'_, '_> {
    pub async fn get_eth_txs_history_entries_max_id(&mut self) -> usize {
        sqlx::query!(
            r#"
            SELECT
                MAX(id)
            FROM
                eth_txs_history
            "#
        )
        .fetch_one(self.storage.conn())
        .await
        .unwrap()
        .max
        .unwrap()
        .try_into()
        .unwrap()
    }

    pub async fn get_last_sent_successfully_eth_tx_by_batch_and_op(
        &mut self,
        l1_batch_number: L1BatchNumber,
        op_type: L1BatchAggregatedActionType,
    ) -> Option<TxHistory> {
        let eth_tx_id = self
            .get_eth_tx_id_by_batch_and_op(l1_batch_number, op_type)
            .await;
        self.get_last_sent_successfully_eth_tx(eth_tx_id.unwrap()?)
            .await
            .unwrap()
    }

    pub async fn is_using_blobs_in_latest_batch(&mut self) -> DalResult<bool> {
        Ok(sqlx::query!(
            r#"
            SELECT blob_sidecar IS NOT NULL AS "is_using_blobs"
            FROM eth_txs
            WHERE id = (
                SELECT MAX(eth_commit_tx_id)
                FROM l1_batches
                WHERE
                    eth_commit_tx_id IS NOT NULL
                    AND (
                        SELECT pubdata_type
                        FROM miniblocks
                        WHERE l1_batch_number = l1_batches.number
                        ORDER BY miniblocks.number
                        LIMIT 1
                    ) = 'Rollup'
            )
            "#
        )
        .instrument("is_using_blobs_in_latest_batch")
        .fetch_optional(self.storage)
        .await?
        .map(|row| row.is_using_blobs.unwrap_or(false))
        .unwrap_or(false))
    }
}
