use std::collections::HashMap;

use anchor_lang::prelude::Pubkey;
use kamino_lending::Reserve;

use crate::accounts::MarketAccounts;

pub fn get_all_reserve_mints(
    market_accounts: &HashMap<Pubkey, MarketAccounts>,
) -> (HashMap<Pubkey, Reserve>, Vec<Pubkey>, Vec<Pubkey>) {
    let mut all_reserves = HashMap::new();
    let mut c_mints = Vec::new();
    let mut l_mints = Vec::new();
    for (_, market_account) in market_accounts.iter() {
        for (_, reserve) in market_account.reserves.iter() {
            all_reserves.insert(reserve.liquidity.mint_pubkey, *reserve);
            c_mints.push(reserve.collateral.mint_pubkey);
            l_mints.push(reserve.liquidity.mint_pubkey);
        }
    }
    (all_reserves, c_mints, l_mints)
}
