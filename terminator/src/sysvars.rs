use anchor_lang::{prelude::Clock, solana_program::sysvar::SysvarId};
use anyhow::Result;
use orbit_link::async_client::AsyncClient;

/// Get current clock
pub async fn get_clock(rpc: &impl AsyncClient) -> Result<Clock> {
    let clock = rpc.get_account(&Clock::id()).await?.deserialize_data()?;

    Ok(clock)
}

/// Get current clock
pub async fn clock(rpc: &impl AsyncClient) -> Clock {
    get_clock(rpc).await.unwrap()
}
