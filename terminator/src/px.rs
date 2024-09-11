use std::{collections::HashMap, str::FromStr};

use anchor_lang::prelude::Pubkey;
use anyhow::Result;
use juno::SwapPrice;
use tracing::info;

pub struct Prices {
    pub prices: HashMap<Pubkey, f64>,
}

impl Prices {
    pub fn a_to_b(&self, a: &Pubkey, b: &Pubkey) -> f64 {
        info!("Getting price of {} to {}", a, b);
        let a_price = self.prices.get(a).unwrap();
        let b_price = self.prices.get(b).unwrap();

        a_price / b_price
    }
}

pub async fn fetch_jup_prices(
    input_mints: &[Pubkey],
    output_mint: &Pubkey,
    amount: f32,
) -> Result<Prices> {
    let raw_prices = juno::get_prices(input_mints, output_mint, amount).await?;
    let mut prices: HashMap<Pubkey, f64> = HashMap::new();
    for (mint, SwapPrice { price, .. }) in raw_prices {
        prices.insert(Pubkey::from_str(&mint).unwrap(), price as f64);
    }
    Ok(Prices { prices })
}
