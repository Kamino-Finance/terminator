use std::{collections::HashMap, sync::Arc};

use anchor_client::solana_client::nonblocking::rpc_client::RpcClient;
use anchor_lang::{prelude::Pubkey, solana_program::program_pack::Pack, AccountDeserialize, Id};
use anchor_spl::token::{Mint, Token};
use anyhow::{anyhow, Result};
use kamino_lending::Reserve;
use solana_sdk::{signature::Keypair, signer::Signer};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use spl_token::state::Account as TokenAccount;
use tracing::{debug, info, warn};

use crate::{accounts::find_account, client::KlendClient, consts::WRAPPED_SOL_MINT, px::Prices};

#[derive(Debug, Clone, Default)]
pub struct Holdings {
    pub holdings: Vec<Holding>,
    pub sol: Holding,
}

#[derive(Debug, Clone, Default)]
pub struct Holding {
    pub mint: Pubkey,
    pub ata: Pubkey,
    pub decimals: u8,
    pub balance: u64,
    pub ui_balance: f64,
    pub label: String,
    pub usd_value: f64,
}

impl Holdings {
    pub fn holding_of(&self, mint: &Pubkey) -> Result<Holding> {
        for holding in self.holdings.iter() {
            if holding.mint == *mint {
                return Ok(holding.clone());
            }
        }
        Err(anyhow!("Holding not found for mint {}", mint))
    }
}

#[derive(Debug, Clone)]
pub struct Liquidator {
    pub wallet: Arc<Keypair>,
    pub atas: HashMap<Pubkey, Pubkey>,
}

fn label_of(mint: &Pubkey, reserves: &HashMap<Pubkey, Reserve>) -> String {
    for (_, reserve) in reserves.iter() {
        if &reserve.liquidity.mint_pubkey == mint {
            let symbol = reserve.config.token_info.symbol().to_string();
            if symbol == "SOL" {
                return "WSOL".to_string();
            } else {
                return symbol;
            }
        }
    }
    mint.to_string()
}

impl Liquidator {
    pub async fn init(
        client: &KlendClient,
        reserves: &HashMap<Pubkey, Reserve>,
    ) -> Result<Liquidator> {
        // Load reserves mints
        let mints: Vec<Pubkey> = reserves
            .iter()
            .flat_map(|(_, r)| [r.liquidity.mint_pubkey, r.collateral.mint_pubkey])
            .collect();
        // Load wallet
        let mut atas = HashMap::new();
        let wallet = match { client.client.payer().ok() } {
            Some(wallet) => {
                // Load or create atas
                info!("Loading atas...");
                let wallet = Arc::new(wallet.insecure_clone());
                let get_or_create_atas_futures = mints
                    .iter()
                    .map(|mint| get_or_create_ata(client, &wallet, mint));
                let get_or_create_atas =
                    futures::future::join_all(get_or_create_atas_futures).await;
                for (i, ata) in get_or_create_atas.into_iter().enumerate() {
                    atas.insert(*mints.get(i).unwrap(), ata?);
                }
                info!(
                    "Loaded liquidator {} with {} tokens",
                    wallet.pubkey(),
                    atas.len(),
                );
                wallet
            }
            None => Arc::new(Keypair::new()),
        };

        let liquidator = Liquidator { wallet, atas };

        Ok(liquidator)
    }

    pub async fn fetch_holdings(
        &self,
        client: &RpcClient,
        reserves: &HashMap<Pubkey, Reserve>,
        prices: &Prices,
    ) -> Result<Holdings> {
        let mut holdings = Vec::new();
        // TODO: optimize this, get all accounts in batch to have 1 single rpc call
        let get_token_balance_futures = self
            .atas
            .iter()
            .map(|(mint, ata)| get_token_balance(client, mint, ata));
        let get_token_balance = futures::future::join_all(get_token_balance_futures).await;
        for (i, (mint, ata)) in self.atas.iter().enumerate() {
            match get_token_balance.get(i) {
                Some(Ok((balance, decimals))) => {
                    let ui_balance = *balance as f64 / 10u64.pow(*decimals as u32) as f64;
                    holdings.push(Holding {
                        mint: *mint,
                        ata: *ata,
                        decimals: *decimals,
                        balance: *balance,
                        ui_balance,
                        label: label_of(mint, reserves),
                        usd_value: if *balance > 0 {
                            prices
                                .prices
                                .get(mint)
                                .map_or(0.0, |price| ui_balance * price)
                        } else {
                            0.0
                        },
                    });
                }
                Some(Err(e)) => {
                    warn!(
                        "Error getting balance for mint {:?} and ata {:?}: {:?}",
                        mint, ata, e
                    );
                }
                None => {
                    warn!("No result for mint {:?} and ata {:?}", mint, ata);
                }
            }
        }

        // Load SOL balance
        let balance = client.get_balance(&self.wallet.pubkey()).await.unwrap();
        let ui_balance = balance as f64 / 10u64.pow(9) as f64;
        let sol_holding = Holding {
            mint: Pubkey::default(), // No mint, this is the native balance
            ata: Pubkey::default(),  // Holding in the native account, not in the ata
            decimals: 9,
            balance,
            ui_balance,
            label: "SOL".to_string(),
            usd_value: ui_balance * prices.prices.get(&WRAPPED_SOL_MINT).unwrap(),
        };
        info!("Holding {} SOL", sol_holding.ui_balance);

        for holding in holdings.iter() {
            if holding.balance > 0 {
                info!("Holding {} {}", holding.ui_balance, holding.label);
            }
        }

        let holding = Holdings {
            holdings,
            sol: sol_holding,
        };
        Ok(holding)
    }
}

async fn get_or_create_ata(
    client: &KlendClient,
    owner: &Arc<Keypair>,
    mint: &Pubkey,
) -> Result<Pubkey> {
    let owner_pubkey = &owner.pubkey();
    let ata = get_associated_token_address(owner_pubkey, mint);
    if !matches!(find_account(&client.client.client, ata).await, Ok(None)) {
        debug!("Liquidator ATA for mint {} exists: {}", mint, ata);
        Ok(ata)
    } else {
        debug!(
            "Liquidator ATA for mint {} does not exist, creating...",
            mint
        );
        let ix = create_associated_token_account(owner_pubkey, owner_pubkey, mint, &Token::id());
        let tx = client
            .client
            .tx_builder()
            .add_ix(ix)
            .build(&[])
            .await
            .unwrap();

        let (sig, _) = client.send_and_confirm_transaction(tx).await.unwrap();

        debug!(
            "Created ata for liquidator: {}, mint: {}, ata: {}, sig: {:?}",
            owner.pubkey(),
            mint,
            ata,
            sig
        );

        Ok(ata)
    }
}

/// get the balance of a token account
pub async fn get_token_balance(
    client: &RpcClient,
    mint: &Pubkey,
    token_account: &Pubkey,
) -> Result<(u64, u8)> {
    let mint_account = client.get_account(mint).await?;
    let token_account = client.get_account(token_account).await?;
    let token_account = TokenAccount::unpack(&token_account.data)?;
    let mint_account = Mint::try_deserialize_unchecked(&mut mint_account.data.as_ref())?;
    let amount = token_account.amount;
    let decimals = mint_account.decimals;
    Ok((amount, decimals))
}
