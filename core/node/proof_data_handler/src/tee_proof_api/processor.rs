use axum::Json;
use chrono::{Duration, Utc};
use zksync_dal::{
    tee_proof_generation_dal::{LockedBatch, TeeProofGenerationJobStatus},
    CoreDal,
};
use zksync_object_store::ObjectStoreError;
use zksync_prover_interface::{
    api::{
        RegisterTeeAttestationRequest, RegisterTeeAttestationResponse, SubmitProofResponse,
        SubmitTeeProofRequest, TeeProofGenerationDataRequest, TeeProofGenerationDataResponse,
    },
    inputs::{
        TeeVerifierInput, V1TeeVerifierInput, VMRunWitnessInputData, WitnessInputMerklePaths,
    },
};
use zksync_types::{tee_types::TeeType, L1BatchNumber};
use zksync_vm_executor::storage::L1BatchParamsProvider;

use crate::{errors::RequestProcessorError, metrics::METRICS, tee_proof_api::RequestProcessor};

impl RequestProcessor {
    pub(crate) async fn get_tee_proof_generation_data(
        &self,
        request: TeeProofGenerationDataRequest,
    ) -> Result<Json<Option<TeeProofGenerationDataResponse>>, RequestProcessorError> {
        tracing::info!("Received request for proof generation data: {:?}", request);

        let batch_ignored_timeout = Duration::from_std(
            self.config
                .tee_config
                .tee_batch_permanently_ignored_timeout(),
        )
        .map_err(|err| {
            RequestProcessorError::GeneralError(format!(
                "Failed to convert batch_ignored_timeout: {}",
                err
            ))
        })?;
        let min_batch_number = self.config.tee_config.first_tee_processed_batch;

        loop {
            let Some(locked_batch) = self
                .lock_batch_for_tee_proving(request.tee_type, min_batch_number)
                .await?
            else {
                break Ok(Json(None)); // no job available
            };
            let batch_number = locked_batch.l1_batch_number;

            match self
                .tee_verifier_input_for_existing_batch(batch_number)
                .await
            {
                Ok(input) => {
                    break Ok(Json(Some(TeeProofGenerationDataResponse(Box::new(input)))));
                }
                Err(RequestProcessorError::ObjectStore(ObjectStoreError::KeyNotFound(_))) => {
                    let duration = Utc::now().signed_duration_since(locked_batch.created_at);
                    let status = if duration > batch_ignored_timeout {
                        TeeProofGenerationJobStatus::PermanentlyIgnored
                    } else {
                        TeeProofGenerationJobStatus::Failed
                    };
                    self.unlock_tee_batch(batch_number, request.tee_type, status)
                        .await?;
                    tracing::warn!(
                        "Assigned status {} to batch {} created at {}",
                        status,
                        batch_number,
                        locked_batch.created_at
                    );
                }
                Err(err) => {
                    self.unlock_tee_batch(
                        batch_number,
                        request.tee_type,
                        TeeProofGenerationJobStatus::Failed,
                    )
                    .await?;
                    break Err(err);
                }
            }
        }
    }

    #[tracing::instrument(skip(self))]
    async fn tee_verifier_input_for_existing_batch(
        &self,
        l1_batch_number: L1BatchNumber,
    ) -> Result<TeeVerifierInput, RequestProcessorError> {
        let vm_run_data: VMRunWitnessInputData = self
            .blob_store
            .get(l1_batch_number)
            .await
            .map_err(RequestProcessorError::ObjectStore)?;

        let merkle_paths: WitnessInputMerklePaths = self
            .blob_store
            .get(l1_batch_number)
            .await
            .map_err(RequestProcessorError::ObjectStore)?;

        let mut connection = self
            .pool
            .connection_tagged("tee_request_processor")
            .await
            .map_err(RequestProcessorError::Dal)?;

        let l2_blocks_execution_data = connection
            .transactions_dal()
            .get_l2_blocks_to_execute_for_l1_batch(l1_batch_number)
            .await
            .map_err(RequestProcessorError::Dal)?;

        let l1_batch_params_provider = L1BatchParamsProvider::new(&mut connection)
            .await
            .map_err(|err| RequestProcessorError::GeneralError(err.to_string()))?;

        // In the state keeper, this value is used to reject execution.
        // All batches have already been executed by State Keeper.
        // This means we don't want to reject any execution, therefore we're using MAX as an allow all.
        let validation_computational_gas_limit = u32::MAX;

        let (system_env, l1_batch_env, pubdata_params) = l1_batch_params_provider
            .load_l1_batch_env(
                &mut connection,
                l1_batch_number,
                validation_computational_gas_limit,
                self.l2_chain_id,
            )
            .await
            .map_err(|err| RequestProcessorError::GeneralError(err.to_string()))?
            .ok_or(RequestProcessorError::GeneralError(
                "system_env, l1_batch_env missing".into(),
            ))?;

        Ok(TeeVerifierInput::new(V1TeeVerifierInput {
            vm_run_data,
            merkle_paths,
            l2_blocks_execution_data,
            l1_batch_env,
            system_env,
            pubdata_params,
        }))
    }

    async fn lock_batch_for_tee_proving(
        &self,
        tee_type: TeeType,
        min_batch_number: L1BatchNumber,
    ) -> Result<Option<LockedBatch>, RequestProcessorError> {
        self.pool
            .connection_tagged("tee_request_processor")
            .await?
            .tee_proof_generation_dal()
            .lock_batch_for_proving(
                tee_type,
                self.config.tee_config.tee_proof_generation_timeout(),
                min_batch_number,
            )
            .await
            .map_err(RequestProcessorError::Dal)
    }

    async fn unlock_tee_batch(
        &self,
        l1_batch_number: L1BatchNumber,
        tee_type: TeeType,
        status: TeeProofGenerationJobStatus,
    ) -> Result<(), RequestProcessorError> {
        self.pool
            .connection_tagged("tee_request_processor")
            .await?
            .tee_proof_generation_dal()
            .unlock_batch(l1_batch_number, tee_type, status)
            .await?;
        Ok(())
    }

    pub(crate) async fn submit_tee_proof(
        &self,
        l1_batch_number: L1BatchNumber,
        proof: SubmitTeeProofRequest,
    ) -> Result<Json<SubmitProofResponse>, RequestProcessorError> {
        let mut connection = self.pool.connection_tagged("tee_request_processor").await?;
        let mut dal = connection.tee_proof_generation_dal();

        dal.save_proof_artifacts_metadata(
            l1_batch_number,
            proof.0.tee_type,
            &proof.0.pubkey,
            &proof.0.signature,
            &proof.0.proof,
        )
        .await?;

        let sealed_at = connection
            .blocks_dal()
            .get_batch_sealed_at(l1_batch_number)
            .await?;

        let duration = sealed_at.and_then(|sealed_at| (Utc::now() - sealed_at).to_std().ok());

        let duration_secs_f64 = if let Some(duration) = duration {
            METRICS.tee_proof_roundtrip_time[&proof.0.tee_type.into()].observe(duration);
            duration.as_secs_f64()
        } else {
            f64::NAN
        };

        tracing::info!(
            l1_batch_number = %l1_batch_number,
            sealed_to_proven_in_secs = duration_secs_f64,
            "Received proof {:?}",
            proof
        );

        Ok(Json(SubmitProofResponse::Success))
    }

    pub(crate) async fn register_tee_attestation(
        &self,
        payload: RegisterTeeAttestationRequest,
    ) -> Result<Json<RegisterTeeAttestationResponse>, RequestProcessorError> {
        tracing::info!("Received attestation: {:?}", payload);

        let mut connection = self.pool.connection_tagged("tee_request_processor").await?;
        let mut dal = connection.tee_proof_generation_dal();

        dal.save_attestation(&payload.pubkey, &payload.attestation)
            .await?;

        Ok(Json(RegisterTeeAttestationResponse::Success))
    }
}
