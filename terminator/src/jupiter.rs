use std::collections::HashSet;

use anchor_lang::prelude::Pubkey;
use juno::{get_quote, get_swap_instructions, AsyncAccountFetcher, DecompiledVersionedTx};
use tracing::warn;

use crate::consts::{
    EXTRA_ACCOUNTS_BUFFER, MAX_ACCOUNTS_PER_TRANSACTION, MAX_EXTRA_ACCOUNTS_BUFFER,
};

pub async fn get_best_swap_route(
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount: u64,
    only_direct_routes: bool,
    slippage_bps: Option<u16>,
    price_impact_limit: Option<f32>,
    max_accounts: Option<u8>,
) -> juno::Result<juno::SwapRoute> {
    let best_route = get_quote(
        input_mint,
        output_mint,
        amount,
        only_direct_routes,
        slippage_bps,
        max_accounts,
    )
    .await?;

    let route_price_impact_pct = best_route
        .price_impact_pct
        .parse::<f32>()
        .map_err(|_| juno::Error::ResponseTypeConversionError)?;
    if let Some(price_impact_limit) = price_impact_limit {
        if route_price_impact_pct > price_impact_limit {
            return Err(juno::Error::PriceImpactTooHigh(route_price_impact_pct));
        }
    }
    Ok(best_route)
}

#[allow(clippy::too_many_arguments)]
/// Get the swap instructions for the best route matching parameters
pub async fn get_best_swap_instructions(
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount: u64,
    only_direct_routes: bool,
    slippage_bps: Option<u16>,
    price_impact_limit: Option<f32>,
    user_public_key: Pubkey,
    accounts_fetcher: &impl AsyncAccountFetcher,
    accounts: Option<&Vec<&Pubkey>>,
    accounts_count_buffer: Option<usize>,
) -> juno::Result<DecompiledVersionedTx> {
    // when we use Jup swap + Kamino's swap rewards in a single transaction, the total number of unique accounts that we can lock (include) in the tx is 64. We need to count how many accounts the Kamino ix will use and require Jup API to give us a route that uses less than MAX_ACCOUNTS_PER_TRANSACTION - the amount of accounts that Kamino will use. The Jup API is taking the maxAccounts as a recommandation not as a hard limit so we should have a buffer such as the number of real accounts in the swap ix - buffer size <= maxAccounts. While this is not true we increase the buffer size (so be more aggressive with the maxAccounts parameter passed to the Jup API).
    // Example: SwapRewards ix uses 30 distinct accounts; maxAccounts = 64 - 30 - 5 = 29; the route returned by Jup for 29 maxAccounts has 40 accounts and 36 of them are distinct from the SwapRewards, so 66 accounts locked in total, which is too much. Increase the buffer by to so now maxAccounts = 64 - 30 - 7 = 27. he route returned by Jup for 27 maxAccounts has 35 accounts and 32 of them are distinct from the SwapRewards so 62 accounts locked so the tx will succeed.
    let accounts_count_buffer = accounts_count_buffer.unwrap_or(0);
    let mut extra_accounts_buffer = EXTRA_ACCOUNTS_BUFFER;

    let mut accounts_distict: HashSet<&Pubkey> = HashSet::new();
    if let Some(accounts) = accounts {
        accounts_distict.extend(accounts.iter());
    }
    accounts_distict.insert(&user_public_key);

    let accounts_distict_count = accounts_distict.len();

    while extra_accounts_buffer < MAX_EXTRA_ACCOUNTS_BUFFER {
        let max_accounts = MAX_ACCOUNTS_PER_TRANSACTION
            .saturating_sub(accounts_distict_count)
            .saturating_sub(extra_accounts_buffer)
            .saturating_sub(accounts_count_buffer);

        let best_route = match get_best_swap_route(
            input_mint,
            output_mint,
            amount,
            only_direct_routes,
            slippage_bps,
            price_impact_limit,
            Some(max_accounts.try_into().unwrap()),
        )
        .await
        {
            Ok(res) => Some(res),
            Err(_) => None,
        };

        if let Some(best_route) = best_route {
            let instructions_result =
                get_swap_instructions(best_route, user_public_key, accounts_fetcher).await;
            if let Ok(decompiled_tx) = instructions_result {
                let total_accounts = decompiled_tx
                    .instructions
                    .iter()
                    .flat_map(|ix| ix.accounts.iter().map(|a| &a.pubkey))
                    .chain(accounts_distict.iter().copied())
                    .collect::<HashSet<_>>();
                if total_accounts.len() <= MAX_ACCOUNTS_PER_TRANSACTION {
                    println!("max accounts {}", max_accounts);
                    return Ok(decompiled_tx);
                }
            }
        } else {
            warn!("cannot find route from {input_mint} to {output_mint} for max_accounts {max_accounts}");
            return Err(juno::Error::NoValidRoute);
        }

        extra_accounts_buffer += 2;
    }

    Err(juno::Error::NoValidRoute)
}
