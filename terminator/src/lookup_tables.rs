use std::collections::{HashMap, HashSet};

use anchor_lang::prelude::Pubkey;
use kamino_lending::{LendingMarket, Reserve};

use crate::liquidator::Liquidator;

pub fn collect_keys(
    reserves: &HashMap<Pubkey, Reserve>,
    liquidator: &Liquidator,
    lending_market: &LendingMarket,
) -> HashSet<Pubkey> {
    let mut lending_markets = HashSet::new();
    let mut keys = HashSet::new();
    for (pubkey, reserve) in reserves {
        keys.insert(*pubkey);
        keys.insert(reserve.collateral.supply_vault);
        keys.insert(reserve.collateral.mint_pubkey);
        keys.insert(reserve.liquidity.supply_vault);
        keys.insert(reserve.liquidity.fee_vault);
        keys.insert(reserve.liquidity.mint_pubkey);
        lending_markets.insert(reserve.lending_market);
    }

    for (mint, ata) in liquidator.atas.iter() {
        keys.insert(*mint);
        keys.insert(*ata);
    }

    keys.insert(lending_market.lending_market_owner);
    keys.insert(lending_market.risk_council);

    for lending_market in lending_markets.iter() {
        let lending_market_authority =
            kamino_lending::utils::seeds::pda::lending_market_auth(lending_market);
        keys.insert(*lending_market);
        keys.insert(lending_market_authority);
    }

    keys
}
