use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context as AnyhowContext, Result};
use backoff::{
    future::retry_notify, Error as BackoffError, ExponentialBackoff, ExponentialBackoffBuilder,
};
use ethers::prelude::*;

use tracing::{error, info, warn};

use crate::{
    types::{BlobEntity, BlockData, BlockEntity, TransactionEntity},
    utils::{context::Context, web3::calculate_versioned_hash},
};

pub struct SlotProcessorOptions {
    pub backoff_config: ExponentialBackoff,
}

pub struct SlotProcessor<'a> {
    options: SlotProcessorOptions,
    context: &'a Context,
}

impl<'a> SlotProcessor<'a> {
    pub async fn try_init(
        context: &'a Context,
        options: Option<SlotProcessorOptions>,
    ) -> Result<SlotProcessor> {
        let options = options.unwrap_or(SlotProcessorOptions {
            backoff_config: ExponentialBackoffBuilder::default()
                .with_initial_interval(Duration::from_secs(2))
                .with_max_elapsed_time(Some(Duration::from_secs(60)))
                .build(),
        });

        Ok(Self { options, context })
    }

    pub async fn process_slots(&mut self, start_slot: u32, end_slot: u32) -> Result<()> {
        let mut current_slot = start_slot;

        while current_slot < end_slot {
            let result = self.process_slot_with_retry(current_slot).await;

            if let Err(e) = result {
                self.save_slot(current_slot - 1).await?;

                error!("[Slot {current_slot}] Couldn't process slot: {e}");

                return Err(e);
            };

            current_slot += 1;
        }

        self.save_slot(current_slot).await?;

        Ok(())
    }

    async fn process_slot_with_retry(&self, slot: u32) -> Result<()> {
        let backoff_config = self.options.backoff_config.clone();

        /*
          This is necessary because the `retry` function requires
          the closure to be `FnMut` and the `SlotProcessor` instance is not `Clone`able. The `Arc<Mutex<>>` allows us to
          share the `SlotProcessor` instance across multiple tasks and safely mutate it within the context of the retry loop.
        */
        let shared_slot_processor = Arc::new(Mutex::new(self));

        retry_notify(
            backoff_config,
            || {
                let slot_processor = Arc::clone(&shared_slot_processor);

                /*
                 Using unwrap() here. If Mutex is poisoned due to a panic, it returns an error. 
                 In this case, we allow the indexer to crash as the state might be invalid. 
                */
                async move {
                    let slot_processor = slot_processor.lock().unwrap();

                    match slot_processor.process_slot(slot).await {
                        Ok(_) => Ok(()),
                        Err(e) => Err(e),
                    }
                }
            },
            |e, duration: Duration| {
                let duration = duration.as_secs();
                warn!("[Slot {slot}] Slot processing failed. Retrying in {duration} seconds… (Reason: {e})");
            },
        )
        .await
    }

    pub async fn process_slot(&self, slot: u32) -> Result<(), backoff::Error<anyhow::Error>> {
        let Context {
            beacon_client,
            blobscan_client,
            provider,
        } = self.context;

        let start = Instant::now();

        let beacon_block = match beacon_client
            .get_block(Some(slot))
            .await
            .map_err(|err| BackoffError::transient(anyhow::Error::new(err)))?
        {
            Some(block) => block,
            None => {
                info!("[Slot {slot}] Skipping as there is no beacon block");

                return Ok(());
            }
        };

        let execution_payload = match beacon_block.body.execution_payload {
            Some(payload) => payload,
            None => {
                info!("[Slot {slot}] Skipping as beacon block doesn't contain execution payload");

                return Ok(());
            }
        };

        match beacon_block.body.blob_kzg_commitments {
            Some(commitments) => commitments,
            None => {
                info!(
                    "[Slot {slot}] Skipping as beacon block doesn't contain blob kzg commitments"
                );

                return Ok(());
            }
        };

        let execution_block_hash = execution_payload.block_hash;

        let execution_block = provider
            .get_block_with_txs(execution_block_hash)
            .await
            .with_context(|| format!("Failed to fetch execution block {execution_block_hash}"))?
            .with_context(|| format!("Execution block {execution_block_hash} not found"))
            .map_err(BackoffError::Permanent)?;

        let block_data =
            BlockData::try_from((&execution_block, slot)).map_err(BackoffError::Permanent)?;

        if block_data.tx_to_versioned_hashes.is_empty() {
            info!("[Slot {slot}] Skipping as execution block doesn't contain blob txs");

            return Ok(());
        }

        let blobs = match beacon_client
            .get_blobs(slot)
            .await
            .map_err(|err| BackoffError::transient(anyhow::Error::new(err)))?
        {
            Some(blobs) => {
                if blobs.is_empty() {
                    info!("[Slot {slot}] Skipping as blobs sidecar is empty");

                    return Ok(());
                } else {
                    blobs
                }
            }
            None => {
                info!("[Slot {slot}] Skipping as there is no blobs sidecar");

                return Ok(());
            }
        };

        let execution_block_number = execution_block.number.with_context(|| {
            format!("Missing block number field in execution block {execution_block_hash}")
        })?;

        let block_entity = BlockEntity {
            hash: execution_block_hash,
            slot,
            number: execution_block_number,
            timestamp: execution_block.timestamp,
        };

        let transactions_entities = block_data
            .block
            .transactions
            .iter()
            .filter(|tx| block_data.tx_to_versioned_hashes.contains_key(&tx.hash))
            .map(|tx| {
                let hash = tx.hash;
                let to = tx
                    .to
                    .with_context(|| format!("Missing to field in transaction {hash}"))?;

                Ok(TransactionEntity {
                    block_number: execution_block_number,
                    from: tx.from,
                    to,
                    hash,
                })
            })
            .collect::<Result<Vec<TransactionEntity>>>()?;

        let blobs_entities = blobs
            .iter()
            .map(|blob| {
                // Need to clone it as it's not possible to have a struct containing a reference field
                // as serde can't serialize it.
                let data = blob.blob.clone();
                let commitment = blob.kzg_commitment.clone();
                let versioned_hash = calculate_versioned_hash(&commitment)?;
                let tx_hash = block_data.tx_to_versioned_hashes.iter().find_map(
                    |(tx_hash, versioned_hashes)| match versioned_hashes.contains(&versioned_hash) {
                        true => Some(tx_hash),
                        false => None,
                    },
                ).with_context(|| format!("No blob transaction found for commitment {commitment} and versioned hash {versioned_hash}"))?;

                Ok(BlobEntity {
                    versioned_hash,
                    commitment,
                    data,
                    index: blob.index.parse()?,
                    tx_hash: *tx_hash,
                })
            })
            .collect::<Result<Vec<BlobEntity>>>()?;

        blobscan_client
            .index(block_entity, transactions_entities, blobs_entities)
            .await
            .map_err(|err| BackoffError::transient(anyhow::Error::new(err)))?;

        let duration = start.elapsed();

        info!(
            "[Slot {slot}] Blobs indexed correctly (elapsed time: {:?}s)",
            duration.as_secs()
        );

        Ok(())
    }

    async fn save_slot(&mut self, slot: u32) -> Result<()> {
        self.context.blobscan_client.update_slot(slot).await?;

        Ok(())
    }
}
