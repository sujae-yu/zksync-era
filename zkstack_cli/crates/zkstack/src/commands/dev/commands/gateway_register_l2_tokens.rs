use std::sync::Arc;

use clap::Parser;
use ethers::{contract::abigen, signers::Signer};
use zksync_types::{Address, L2_NATIVE_TOKEN_VAULT_ADDRESS};

use super::{
    events_gatherer::DEFAULT_BLOCK_RANGE,
    gateway::{check_l2_ntv_existence, get_deployed_by_bridge, get_ethers_provider, get_zk_client},
};

// L2WrappedBaseTokenStore ABI
abigen!(
    L2NativeTokenVaultAbi,
    r"[
    function assetId(address)(bytes32)
    function setLegacyTokenAssetId(address _l2TokenAddress) public
    function L2_LEGACY_SHARED_BRIDGE()(address)
]"
);

abigen!(
    L2LegacySharedBridgeAbi,
    r"[
    function l1TokenAddress(address)(address)
]"
);

pub async fn migrate_l2_tokens(
    private_key: String,
    l2_rpc_url: String,
    l2_chain_id: u64,
    l2_tokens_indexing_block_range: Option<u64>,
) -> anyhow::Result<()> {
    let l2_client = get_zk_client(&l2_rpc_url, l2_chain_id)?;

    check_l2_ntv_existence(&l2_client).await?;
    let ethers_provider = get_ethers_provider(&l2_rpc_url)?;

    let wallet = private_key.parse::<ethers::signers::LocalWallet>()?;
    let wallet = wallet.with_chain_id(l2_chain_id);
    let middleware = ethers::middleware::SignerMiddleware::new(ethers_provider.clone(), wallet);

    let l2_native_token_vault =
        L2NativeTokenVaultAbi::new(L2_NATIVE_TOKEN_VAULT_ADDRESS, Arc::new(middleware));
    let l2_legacy_shared_bridge_addr = l2_native_token_vault.l2_legacy_shared_bridge().await?;
    if l2_legacy_shared_bridge_addr == Address::zero() {
        println!("Chain does not have a legacy bridge. Nothing to migrate");
        return Ok(());
    }

    let l2_legacy_shared_bridge =
        L2LegacySharedBridgeAbi::new(l2_legacy_shared_bridge_addr, ethers_provider);

    let all_tokens = get_deployed_by_bridge(
        &l2_rpc_url,
        l2_legacy_shared_bridge_addr,
        l2_tokens_indexing_block_range.unwrap_or(DEFAULT_BLOCK_RANGE),
    )
    .await?;

    for token in all_tokens {
        let current_asset_id = l2_native_token_vault.asset_id(token).await?;
        // Let's double check whether the token can be registered at all
        let l1_address = l2_legacy_shared_bridge.l_1_token_address(token).await?;

        if current_asset_id == [0u8; 32] && l1_address != Address::zero() {
            println!("Token {:#?} is not registered. Registering...", token);

            let call =
                l2_native_token_vault.method::<_, Address>("setLegacyTokenAssetId", token)?;
            let pending_tx = call.send().await?;

            let receipt = pending_tx.await?;

            if let Some(receipt) = receipt {
                println!(
                    "Transaction {:#?} included in tx: {:?}",
                    receipt.transaction_hash, receipt.block_number
                );
            } else {
                anyhow::bail!("Transaction failed or was dropped.");
            }
        }
    }

    Ok(())
}

#[derive(Parser, Debug, Clone)]
pub struct GatewayRegisterL2TokensArgs {
    chain_id: u64,
    l2_rpc_url: String,
    private_key: String,
    l2_tokens_indexing_block_range: Option<u64>,
}

pub(crate) async fn run(args: GatewayRegisterL2TokensArgs) -> anyhow::Result<()> {
    println!("Looking for unregistered tokens...");

    migrate_l2_tokens(
        args.private_key,
        args.l2_rpc_url,
        args.chain_id,
        args.l2_tokens_indexing_block_range,
    )
    .await?;

    println!("All tokens registered!");

    Ok(())
}
