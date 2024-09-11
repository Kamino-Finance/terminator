use std::collections::{HashMap, HashSet};

use anchor_client::{
    solana_client::{
        nonblocking::rpc_client::RpcClient,
        rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
    },
    solana_sdk::{account::Account, account_info::AccountInfo, pubkey::Pubkey, signer::Signer},
};
use anchor_lang::Id;
use anchor_spl::token::Token;
use anyhow::Result;
use kamino_lending::{LendingMarket, Reserve};
use orbit_link::{async_client::AsyncClient, OrbitLink};
use spl_associated_token_account::instruction::create_associated_token_account;
use tracing::info;

use crate::{
    client::{rpc, KlendClient},
    consts::WRAPPED_SOL_MINT,
};

pub fn create_is_signer_account_infos<'a>(
    accounts: &'a mut [(Pubkey, bool, &'a mut Account)],
) -> HashMap<Pubkey, AccountInfo<'a>> {
    accounts
        .iter_mut()
        .map(|(key, is_signer, account)| {
            (
                *key,
                AccountInfo::new(
                    key,
                    *is_signer,
                    false,
                    &mut account.lamports,
                    &mut account.data,
                    &account.owner,
                    account.executable,
                    account.rent_epoch,
                ),
            )
        })
        .collect()
}

pub struct MarketAccounts {
    pub reserves: HashMap<Pubkey, Reserve>,
    pub lending_market: LendingMarket,
}

pub struct OracleAccounts {
    pub pyth_accounts: Vec<(Pubkey, bool, Account)>,
    pub switchboard_accounts: Vec<(Pubkey, bool, Account)>,
    pub scope_price_accounts: Vec<(Pubkey, bool, Account)>,
}

pub async fn market_and_reserve_accounts(
    client: &KlendClient,
    lending_market: &Pubkey,
) -> Result<MarketAccounts> {
    let market = client
        .client
        .get_anchor_account::<LendingMarket>(lending_market)
        .await?;

    let filter = RpcFilterType::Memcmp(Memcmp::new(
        32,
        MemcmpEncodedBytes::Bytes(lending_market.to_bytes().to_vec()),
    ));
    let filters = vec![filter];

    let res: Vec<(Pubkey, Reserve)> =
        rpc::get_zero_copy_pa(&client.client, &client.program_id, &filters).await?;

    let reserves: HashMap<Pubkey, Reserve> = res.into_iter().collect();

    Ok(MarketAccounts {
        reserves,
        lending_market: market,
    })
}

pub async fn oracle_accounts<T: AsyncClient, S: Signer>(
    client: &OrbitLink<T, S>,
    reserves: &HashMap<Pubkey, Reserve>,
) -> Result<OracleAccounts> {
    let mut pyth_accounts = HashSet::new();
    let mut switchboard_feeds = HashSet::new();
    let mut scope_prices = HashSet::new();

    for (_, reserve) in reserves.iter() {
        pyth_accounts.insert(reserve.config.token_info.pyth_configuration.price);
        switchboard_feeds.insert(
            reserve
                .config
                .token_info
                .switchboard_configuration
                .price_aggregator,
        );
        switchboard_feeds.insert(
            reserve
                .config
                .token_info
                .switchboard_configuration
                .twap_aggregator,
        );
        scope_prices.insert(reserve.config.token_info.scope_configuration.price_feed);
    }

    let pyth_account_pubkeys: Vec<Pubkey> = pyth_accounts.into_iter().collect();
    let pyth_accounts = client
        .client
        .get_multiple_accounts(pyth_account_pubkeys.as_slice())
        .await?;

    let pyth_accounts_mapped = pyth_accounts
        .into_iter()
        .zip(pyth_account_pubkeys.into_iter())
        .filter(|(account, _)| account.is_some())
        .map(|(account, pubkey)| (pubkey, false, account.unwrap()))
        .collect::<Vec<_>>();

    let switchboard_feed_pubkeys: Vec<Pubkey> = switchboard_feeds.into_iter().collect();
    let switchboard_feeds = client
        .client
        .get_multiple_accounts(switchboard_feed_pubkeys.as_slice())
        .await?;

    let switchboard_feeds_mapped = switchboard_feeds
        .into_iter()
        .zip(switchboard_feed_pubkeys.into_iter())
        .filter(|(account, _)| account.is_some())
        .map(|(account, pubkey)| (pubkey, false, account.unwrap()))
        .collect::<Vec<_>>();

    let scope_price_pubkeys: Vec<Pubkey> = scope_prices.into_iter().collect();
    let scope_prices = client
        .client
        .get_multiple_accounts(scope_price_pubkeys.as_slice())
        .await?;

    let scope_prices_mapped = scope_prices
        .into_iter()
        .zip(scope_price_pubkeys.into_iter())
        .filter(|(account, _)| account.is_some())
        .map(|(account, pubkey)| (pubkey, false, account.unwrap()))
        .collect::<Vec<_>>();

    Ok(OracleAccounts {
        pyth_accounts: pyth_accounts_mapped,
        switchboard_accounts: switchboard_feeds_mapped,
        scope_price_accounts: scope_prices_mapped,
    })
}

#[macro_export]
macro_rules! map_and_collect_accounts {
    ($accounts:expr) => {{
        $accounts
            .iter_mut()
            .map(|(pk, writable, acc)| (*pk, *writable, acc))
            .collect::<Vec<_>>()
    }};
}

pub fn map_accounts_and_create_infos(
    accounts: &mut [(Pubkey, bool, Account)],
) -> HashMap<Pubkey, AccountInfo> {
    accounts
        .iter_mut()
        .map(|(key, is_signer, account)| {
            (
                *key,
                AccountInfo::new(
                    key,
                    *is_signer,
                    false,
                    &mut account.lamports,
                    &mut account.data,
                    &account.owner,
                    account.executable,
                    account.rent_epoch,
                ),
            )
        })
        .collect()
}

pub async fn unwrap_wsol_ata(klend_client: &KlendClient) -> Result<String> {
    info!("Unwrapping sol..");
    let user = klend_client.liquidator.wallet.pubkey();

    // Close the account
    let instructions = vec![spl_token::instruction::close_account(
        &Token::id(),
        klend_client.liquidator.atas.get(&WRAPPED_SOL_MINT).unwrap(),
        &user,
        &user,
        &[],
    )?];

    // // Sync remaining sol (no need to do this upon close, on open only)
    // info!("Sync native for wsol ata {}", wsol_ata);
    // let wsol_ata = klend_client.liquidator.atas.get(&WRAPPED_SOL_MINT).unwrap();
    // instructions.push(instruction::sync_native(&Token::id(), &wsol_ata).unwrap());

    // Then create it again so we have wsol ata existing
    let recreate_ix =
        create_associated_token_account(&user, &user, &WRAPPED_SOL_MINT, &Token::id());

    let tx = klend_client
        .client
        .tx_builder()
        .add_ixs(instructions)
        .add_ix(recreate_ix)
        .build(&[])
        .await?;

    let (sig, _) = klend_client.client.send_and_confirm_transaction(tx).await?;

    info!("Executed unwrap transaction: {:?}", sig);
    Ok(sig.to_string())
}

pub async fn find_account(
    client: &RpcClient,
    address: Pubkey,
) -> Result<Option<(Pubkey, Account)>> {
    let res = client.get_account(&address).await;
    if let Ok(account) = res {
        Ok(Some((address, account)))
    } else {
        println!("Ata not found: {}", address);
        Ok(None)
    }
}
