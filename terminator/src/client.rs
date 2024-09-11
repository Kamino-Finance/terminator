use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{Read, Write},
    str::FromStr,
    sync::Arc,
    thread,
    time::Duration,
};

type TransactionResult = std::result::Result<(), TransactionError>;

use anchor_client::{
    solana_client::{
        nonblocking::rpc_client::RpcClient,
        rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
    },
    solana_sdk::pubkey::Pubkey,
};
use anyhow::{anyhow, Result};
use kamino_lending::{
    utils::seeds, LendingMarket, Obligation, ReferrerTokenState, Reserve, ReserveFarmKind,
};
use orbit_link::OrbitLink;
use solana_account_decoder::parse_address_lookup_table::UiLookupTable;
use solana_sdk::{
    address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount},
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    signature::{Keypair, Signature},
    transaction::{TransactionError, VersionedTransaction},
};
use tokio::task;
use tracing::info;

use crate::{
    accounts::{find_account, market_and_reserve_accounts, MarketAccounts},
    consts::{NULL_PUBKEY, WRAPPED_SOL_MINT},
    instructions::{self, InstructionBlocks},
    liquidator::Liquidator,
    lookup_tables::collect_keys,
    model::StateWithKey,
    px,
    px::Prices,
};

pub struct KlendClient {
    pub program_id: Pubkey,

    pub client: OrbitLink<RpcClient, Keypair>,

    // Txn data
    pub lookup_table: Option<AddressLookupTableAccount>,

    // Rebalance settings
    pub rebalance_config: Option<RebalanceConfig>,

    // Liquidator
    // TODO: move all the fields of the liquidator out of this struct and flatten it
    pub liquidator: Liquidator,
}

#[derive(Debug, Clone)]
pub struct RebalanceConfig {
    pub base_token: Pubkey,
    pub min_sol_balance: f64,
    pub usdc_mint: Pubkey,
    pub rebalance_slippage_pct: f64,
    pub non_swappable_dust_usd_value: f64,
}

impl KlendClient {
    pub fn init(
        client: OrbitLink<RpcClient, Keypair>,
        program_id: Pubkey,
        rebalance_config: Option<RebalanceConfig>,
    ) -> Result<Self> {
        // Create a dumb one first
        let liquidator = Liquidator {
            wallet: Arc::new(Keypair::new()),
            atas: HashMap::new(),
        };

        Ok(Self {
            program_id,
            client,
            lookup_table: None,
            liquidator,
            rebalance_config,
        })
    }

    pub async fn fetch_market_and_reserves(&self, market: &Pubkey) -> Result<MarketAccounts> {
        market_and_reserve_accounts(self, market).await
    }

    pub async fn fetch_obligations(&self, market: &Pubkey) -> Result<Vec<(Pubkey, Obligation)>> {
        info!("Fetching obligations for market: {}", market);
        let filter = RpcFilterType::Memcmp(Memcmp::new(
            32,
            MemcmpEncodedBytes::Bytes(market.to_bytes().to_vec()),
        ));
        let filters = vec![filter];
        let obligations = rpc::get_zero_copy_pa(&self.client, &self.program_id, &filters).await?;
        Ok(obligations)
    }

    pub async fn fetch_obligation(&self, obligation_address: &Pubkey) -> Result<Obligation> {
        info!("Fetching obligation: {}", obligation_address);
        let obligation = self
            .client
            .get_anchor_account::<Obligation>(obligation_address)
            .await?;
        Ok(obligation)
    }

    pub async fn fetch_referrer_token_states(&self) -> Result<HashMap<Pubkey, ReferrerTokenState>> {
        let states = self
            .client
            .get_all_zero_copy_accounts::<ReferrerTokenState>()
            .await?;
        let map = states.into_iter().collect();
        Ok(map)
    }

    pub async fn load_lookup_table(&mut self, market_accounts: MarketAccounts) {
        self.load_liquidator_lookup_table().await;
        self.update_liquidator_lookup_table(collect_keys(
            &market_accounts.reserves,
            &self.liquidator,
            &market_accounts.lending_market,
        ))
        .await;
        self.client
            .add_lookup_table(self.lookup_table.clone().unwrap());
    }

    async fn load_liquidator_lookup_table(&mut self) {
        // The liquidator has one static lookup table associated with it
        // and is stored on a local file
        // Here we load it or create it and save it
        // we do not manage the addresses, that is done in a separate stage

        let filename = std::env::var("LIQUIDATOR_LOOKUP_TABLE_FILE").unwrap();

        if !std::path::Path::new(&filename).exists() {
            File::create(&filename).unwrap();
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&filename)
            .unwrap();
        let mut current_content = String::new();
        file.read_to_string(&mut current_content).unwrap();

        if current_content.is_empty() {
            let lut = self
                .create_init_reserve_lookup_table(&[], || {
                    thread::sleep(Duration::from_secs(12));
                })
                .await
                .unwrap();
            self.lookup_table = Some(lut.clone());
            file.set_len(0).unwrap();
            file.write_all(lut.key.to_string().as_bytes()).unwrap();
            info!("Created new empty lookup table {}", lut.key);
        } else {
            let lut_key = Pubkey::from_str(&current_content).unwrap();
            info!("Liquidator lookuptable {:?}", lut_key);
            let lookup_table_data = self.client.client.get_account(&lut_key).await.unwrap();
            let lookup_table: UiLookupTable = UiLookupTable::from(
                AddressLookupTable::deserialize(&lookup_table_data.data).unwrap(),
            );
            self.lookup_table = Some(AddressLookupTableAccount {
                key: lut_key,
                addresses: lookup_table
                    .addresses
                    .iter()
                    .map(|x| Pubkey::from_str(x).unwrap())
                    .collect::<Vec<Pubkey>>(),
            });
            info!(
                "Loaded lookup table {} with {} keys",
                lut_key,
                lookup_table.addresses.len()
            );
        }
    }

    async fn update_liquidator_lookup_table(&mut self, expected: HashSet<Pubkey>) {
        if self.lookup_table.is_none() {
            self.load_liquidator_lookup_table().await;
        }

        // TODO: Maybe sleep
        let lut = self.lookup_table.as_ref().unwrap();
        let already_in_lut: HashSet<Pubkey> = HashSet::from_iter(lut.addresses.iter().copied());
        let expected_in_lut: HashSet<Pubkey> = expected;

        let missing_keys = expected_in_lut
            .iter()
            .filter(|x| !already_in_lut.contains(x))
            .copied()
            .collect::<Vec<Pubkey>>();

        let extra_keys = lut
            .addresses
            .iter()
            .filter(|x| !expected_in_lut.contains(*x))
            .copied()
            .collect::<Vec<Pubkey>>();

        info!("Missing keys: {:?}", missing_keys.len());
        info!("Extra keys: {:?}", extra_keys.len());

        if !missing_keys.is_empty() {
            info!("Extending lookup table");
            self.extend_lut_with_keys(lut.key, &missing_keys, || {
                thread::sleep(Duration::from_secs(12));
            })
            .await
            .unwrap();

            // Reload it
            self.load_liquidator_lookup_table().await;
        }
    }

    async fn create_init_reserve_lookup_table(
        &mut self,
        keys: &[Pubkey],
        delay_fn: impl Fn(),
    ) -> Result<AddressLookupTableAccount> {
        use solana_sdk::address_lookup_table::instruction;

        // Create lookup table
        let recent_slot = self
            .client
            .client
            .get_slot_with_commitment(CommitmentConfig::finalized())
            .await?;

        let (create_lookup_table, table_pk) = instruction::create_lookup_table(
            self.client.payer_pubkey(),
            self.client.payer_pubkey(),
            recent_slot,
        );

        let txn = self.client.create_tx(&[create_lookup_table], &[]).await?;

        self.client.send_and_confirm_transaction(txn).await?;

        let keys = keys
            .iter()
            .filter(|x| **x != NULL_PUBKEY)
            .copied()
            .collect::<Vec<Pubkey>>();

        self.extend_lut_with_keys(table_pk, &keys, delay_fn).await?;

        Ok(AddressLookupTableAccount {
            key: table_pk,
            addresses: keys,
        })
    }

    async fn extend_lut_with_keys(
        &self,
        table_pk: Pubkey,
        keys: &[Pubkey],
        delay_fn: impl Fn(),
    ) -> Result<()> {
        use solana_sdk::address_lookup_table::instruction;

        for selected_keys in keys.chunks(20) {
            info!("Extending lookup table with {} keys", selected_keys.len());
            let extend_ix = instruction::extend_lookup_table(
                table_pk,
                self.client.payer_pubkey(),
                Some(self.client.payer_pubkey()),
                selected_keys.to_vec(),
            );

            let tx = self.client.create_tx(&[extend_ix], &[]).await?;
            self.send_and_confirm_transaction(tx).await.unwrap();
            // wait until lookup table is active
            delay_fn();
        }

        Ok(())
    }

    // TODO: move this to orbitlink
    pub async fn send_and_confirm_transaction(
        &self,
        tx: VersionedTransaction,
    ) -> Result<(Signature, Option<TransactionResult>)> {
        let mut num_retries = 0;
        let max_retries = 5;
        loop {
            num_retries += 1;
            if num_retries > max_retries {
                return Err(anyhow!("Max retries reached"));
            }
            let (sig, res) = self.client.send_and_confirm_transaction(tx.clone()).await?;
            if let Some(Err(TransactionError::BlockhashNotFound)) = res {
                continue;
            } else {
                return Ok((sig, res));
            }
        }
    }

    pub async fn fetch_all_prices(
        &mut self,
        reserves: &[Reserve],
        usd_mint: &Pubkey,
    ) -> Result<Prices> {
        let mut mints = reserves
            .iter()
            .map(|x| x.liquidity.mint_pubkey)
            .collect::<HashSet<Pubkey>>();

        if let Some(c) = &self.rebalance_config {
            mints.insert(c.base_token);
            mints.insert(c.usdc_mint);
        };
        mints.insert(WRAPPED_SOL_MINT);

        // Convert mints to vec
        let mints = mints.into_iter().collect::<Vec<Pubkey>>();

        // TOOD: fix amount to be per token
        let amount = 100.0;
        px::fetch_jup_prices(&mints, usd_mint, amount).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn liquidate_obligation_and_redeem_reserve_collateral_ixns(
        &self,
        lending_market: StateWithKey<LendingMarket>,
        debt_reserve: StateWithKey<Reserve>,
        coll_reserve: StateWithKey<Reserve>,
        obligation: StateWithKey<Obligation>,
        liquidity_amount: u64,
        min_acceptable_received_coll_amount: u64,
        max_allowed_ltv_override_pct_opt: Option<u64>,
    ) -> Result<Vec<Instruction>> {
        let liquidate_ix = instructions::liquidate_obligation_and_redeem_reserve_collateral_ix(
            &self.program_id,
            lending_market,
            debt_reserve.clone(),
            coll_reserve.clone(),
            &self.liquidator,
            obligation.key,
            liquidity_amount,
            min_acceptable_received_coll_amount,
            max_allowed_ltv_override_pct_opt,
        );

        let (pre_instructions, post_instructions) = self
            .wrap_obligation_instruction_with_farms(
                &[&coll_reserve, &debt_reserve],
                &[ReserveFarmKind::Collateral, ReserveFarmKind::Debt],
                &obligation,
                &self.liquidator.wallet.clone(),
            )
            .await;

        let mut instructions = vec![];
        for ix in pre_instructions {
            instructions.push(ix.instruction);
        }
        instructions.push(liquidate_ix.instruction);
        for ix in post_instructions {
            instructions.push(ix.instruction);
        }

        Ok(instructions)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn liquidate_obligation_and_redeem_reserve_collateral(
        &mut self,
        lending_market: StateWithKey<LendingMarket>,
        debt_reserve: StateWithKey<Reserve>,
        coll_reserve: StateWithKey<Reserve>,
        obligation: StateWithKey<Obligation>,
        liquidity_amount: u64,
        min_acceptable_received_coll_amount: u64,
        max_allowed_ltv_override_pct_opt: Option<u64>,
    ) -> Result<VersionedTransaction> {
        let instructions = self
            .liquidate_obligation_and_redeem_reserve_collateral_ixns(
                lending_market,
                debt_reserve,
                coll_reserve,
                obligation,
                liquidity_amount,
                min_acceptable_received_coll_amount,
                max_allowed_ltv_override_pct_opt,
            )
            .await?;

        let txn = self.client.tx_builder().add_ixs(instructions);
        let txn_b64 = txn.to_base64();
        println!(
            "Simulation: https://explorer.solana.com/tx/inspector?message={}",
            urlencoding::encode(&txn_b64)
        );

        txn.build_with_budget_and_fee(&[]).await.map_err(Into::into)
    }

    pub async fn wrap_obligation_instruction_with_farms(
        &self,
        reserve_accts: &[&StateWithKey<Reserve>],
        farm_modes: &[ReserveFarmKind],
        obligation: &StateWithKey<Obligation>,
        payer: &Arc<Keypair>,
    ) -> (Vec<InstructionBlocks>, Vec<InstructionBlocks>) {
        // If has farms, also do init farm obligations
        // Always do refresh_reserve
        // Always do refresh_obligation
        // If has farms, also do refresh farms
        // Then this ix
        // If has farms, also do refresh farms

        let mut pre_instructions = vec![];
        let mut post_instructions = vec![];

        let obligation_state = *(obligation.state.borrow());
        let obligation_address = obligation.key;

        let (deposit_reserves, borrow_reserves, referrer_token_states) = self
            .get_obligation_reserves_and_referrer_token_states(&obligation_state)
            .await;

        let mut unique_reserves = deposit_reserves
            .iter()
            .chain(borrow_reserves.iter())
            .filter_map(|x| *x)
            .collect::<Vec<Pubkey>>();
        unique_reserves.sort();
        unique_reserves.dedup();

        let instruction_reserves = reserve_accts.iter().map(|x| x.key).collect::<Vec<Pubkey>>();

        // 1. Build init_obligation_farm if necessary
        for reserve in reserve_accts {
            let (farm_debt, farm_collateral) = {
                let reserve_state = reserve.state.borrow();
                (
                    reserve_state.get_farm(ReserveFarmKind::Debt),
                    reserve_state.get_farm(ReserveFarmKind::Collateral),
                )
            };
            let (obligation_farm_debt, obligation_farm_coll) =
                obligation_farms(&self.client, farm_debt, farm_collateral, obligation_address)
                    .await;

            if farm_debt != Pubkey::default() && obligation_farm_debt.is_none() {
                let init_obligation_farm_ix = instructions::init_obligation_farm_for_reserve_ix(
                    &self.program_id,
                    reserve,
                    farm_debt,
                    &obligation_address,
                    &obligation_state.owner,
                    payer,
                    ReserveFarmKind::Debt,
                );
                println!(
                    "Adding pre-ixn init_obligation_farm_ix current {:?} debt ",
                    farm_debt
                );
                pre_instructions.push(init_obligation_farm_ix.clone());
            }

            if farm_collateral != Pubkey::default() && obligation_farm_coll.is_none() {
                let init_obligation_farm_ix = instructions::init_obligation_farm_for_reserve_ix(
                    &self.program_id,
                    reserve,
                    farm_collateral,
                    &obligation_address,
                    &obligation_state.owner,
                    payer,
                    ReserveFarmKind::Collateral,
                );
                println!(
                    "Adding pre-ixn init_obligation_farm_ix current {:?} coll",
                    farm_collateral
                );
                pre_instructions.push(init_obligation_farm_ix.clone());
            }
        }

        // 2. Build Refresh Reserve (for the non-instruction reserves - i.e. deposit, borrow)
        for reserve_acc in unique_reserves {
            if instruction_reserves.contains(&reserve_acc) {
                continue;
            }
            let reserve: Reserve = self.client.get_anchor_account(&reserve_acc).await.unwrap();
            let refresh_reserve_ix = instructions::refresh_reserve_ix(
                &self.program_id,
                reserve,
                &reserve_acc,
                payer.clone(),
            );
            println!("Adding pre-ixn refresh_reserve unique {:?}", reserve_acc);
            pre_instructions.push(refresh_reserve_ix);
        }

        // 3. Build Refresh Reserve (for the current instruction - i.e. deposit, borrow)
        for reserve_acc in instruction_reserves {
            let reserve: Reserve = self.client.get_anchor_account(&reserve_acc).await.unwrap();
            let refresh_reserve_ix = instructions::refresh_reserve_ix(
                &self.program_id,
                reserve,
                &reserve_acc,
                payer.clone(),
            );
            println!("Adding pre-ixn refresh_reserve current {:?}", reserve_acc);
            pre_instructions.push(refresh_reserve_ix);
        }

        // 4. Build Refresh Obligation
        let refresh_obligation_ix = instructions::refresh_obligation_ix(
            &self.program_id,
            obligation_state.lending_market,
            obligation_address,
            deposit_reserves,
            borrow_reserves,
            referrer_token_states,
            payer.clone(),
        );

        println!("Adding pre-ixn refresh_obligation");
        pre_instructions.push(refresh_obligation_ix);

        for (reserve_acc, farm_mode) in reserve_accts.iter().zip(farm_modes.iter()) {
            let reserve: Reserve = self
                .client
                .get_anchor_account(&reserve_acc.key)
                .await
                .unwrap();

            let farm = reserve.get_farm(*farm_mode);

            // 5.1 Build Refresh Obligation Farms
            if farm != Pubkey::default() {
                let refresh_farms_ix = instructions::refresh_obligation_farm_for_reserve_ix(
                    &self.program_id,
                    reserve_acc,
                    farm,
                    obligation_address,
                    payer,
                    *farm_mode,
                );

                println!("pre_ixs refresh_obligation_farms {:?}", farm);

                pre_instructions.push(refresh_farms_ix.clone());
                post_instructions.push(refresh_farms_ix);
            }
        }

        (pre_instructions, post_instructions)
    }

    pub async fn get_obligation_reserves_and_referrer_token_states(
        &self,
        obligation: &Obligation,
    ) -> (
        Vec<Option<Pubkey>>,
        Vec<Option<Pubkey>>,
        Vec<Option<Pubkey>>,
    ) {
        let deposit_reserves: Vec<Option<Pubkey>> = obligation
            .deposits
            .iter()
            .filter(|x| x.deposit_reserve != Pubkey::default())
            .map(|x| Some(x.deposit_reserve))
            .collect();

        let borrow_reserves: Vec<Option<Pubkey>> = obligation
            .borrows
            .iter()
            .filter(|x| x.borrow_reserve != Pubkey::default())
            .map(|x| Some(x.borrow_reserve))
            .collect();

        let referrer_token_states: Vec<Option<Pubkey>> = if obligation.has_referrer() {
            let mut vec = Vec::with_capacity(borrow_reserves.len());

            for borrow_reserve in borrow_reserves.iter() {
                match borrow_reserve {
                    Some(borrow_reserve) => {
                        let reserve_account: Reserve = self
                            .client
                            .get_anchor_account(borrow_reserve)
                            .await
                            .unwrap();

                        vec.push(Some(get_referrer_token_state_key(
                            &obligation.referrer,
                            &reserve_account.liquidity.mint_pubkey,
                        )));
                    }
                    None => {}
                }
            }
            vec
        } else {
            Vec::new()
        };

        (deposit_reserves, borrow_reserves, referrer_token_states)
    }
}

pub fn get_referrer_token_state_key(referrer: &Pubkey, mint: &Pubkey) -> Pubkey {
    let (referrer_token_state_key, _referrer_token_state_bump) = Pubkey::find_program_address(
        &[
            seeds::BASE_SEED_REFERRER_TOKEN_STATE,
            referrer.as_ref(),
            mint.as_ref(),
        ],
        &kamino_lending::id(),
    );

    referrer_token_state_key
}

pub async fn obligation_farms(
    client: &OrbitLink<RpcClient, Keypair>,
    farm_debt: Pubkey,
    farm_collateral: Pubkey,
    obligation_address: Pubkey,
) -> (
    Option<StateWithKey<farms::state::UserState>>,
    Option<StateWithKey<farms::state::UserState>>,
) {
    let (obligation_farm_debt, _) = Pubkey::find_program_address(
        &[
            farms::utils::consts::BASE_SEED_USER_STATE,
            farm_debt.as_ref(),
            obligation_address.as_ref(),
        ],
        &farms::ID,
    );
    let (obligation_farm_coll, _) = Pubkey::find_program_address(
        &[
            farms::utils::consts::BASE_SEED_USER_STATE,
            farm_collateral.as_ref(),
            obligation_address.as_ref(),
        ],
        &farms::ID,
    );

    let obligation_farm_debt_account = if farm_debt == Pubkey::default() {
        None
    } else if let Ok(None) = find_account(&client.client, obligation_farm_debt).await {
        None
    } else {
        let acc = client
            .get_anchor_account::<farms::state::UserState>(&obligation_farm_debt)
            .await
            .unwrap();

        Some(StateWithKey::new(acc, obligation_farm_debt))
    };

    let obligation_farm_coll_account = if farm_collateral == Pubkey::default() {
        None
    } else if let Ok(None) = find_account(&client.client, obligation_farm_coll).await {
        None
    } else {
        let acc = client
            .get_anchor_account::<farms::state::UserState>(&obligation_farm_coll)
            .await
            .unwrap();

        Some(StateWithKey::new(acc, obligation_farm_coll))
    };

    (obligation_farm_debt_account, obligation_farm_coll_account)
}

pub mod utils {
    use super::*;

    pub async fn fetch_markets_and_reserves(
        client: &Arc<KlendClient>,
        markets: &[Pubkey],
    ) -> anyhow::Result<HashMap<Pubkey, MarketAccounts>> {
        let futures = markets
            .iter()
            .map(|market| {
                let client = client.clone();
                let market = *market;
                task::spawn(async move { client.fetch_market_and_reserves(&market).await })
            })
            .collect::<Vec<_>>();

        let results = futures::future::join_all(futures).await;

        let mut map = HashMap::new();
        for (i, result) in results.into_iter().enumerate() {
            let r = result??;
            map.insert(markets[i], r);
        }
        Ok(map)
    }
}

pub mod rpc {
    use anchor_client::solana_client::{
        rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
        rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
    };
    use anchor_lang::{Discriminator, Owner};
    use bytemuck::{from_bytes, AnyBitPattern};
    use orbit_link::OrbitLink;
    use solana_account_decoder::UiAccountEncoding;
    use solana_rpc_client::nonblocking::rpc_client::RpcClient;

    use super::*;

    pub async fn get_zero_copy_pa<Acc>(
        client: &OrbitLink<RpcClient, Keypair>,
        program_id: &Pubkey,
        filters: &[RpcFilterType],
    ) -> Result<Vec<(Pubkey, Acc)>>
    where
        Acc: AnyBitPattern + Owner + Discriminator,
    {
        let size = u64::try_from(std::mem::size_of::<Acc>() + 8).unwrap();
        let discrim_memcmp = RpcFilterType::Memcmp(Memcmp::new(
            0,
            MemcmpEncodedBytes::Bytes(Acc::discriminator().to_vec()),
        ));
        let mut all_filters = vec![RpcFilterType::DataSize(size), discrim_memcmp];
        for f in filters {
            all_filters.push(f.clone());
        }
        let config = RpcProgramAccountsConfig {
            filters: Some(all_filters),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64Zstd),
                ..RpcAccountInfoConfig::default()
            },
            ..RpcProgramAccountsConfig::default()
        };

        let accs = client
            .client
            .get_program_accounts_with_config(program_id, config)
            .await?;

        let parsed_accounts = accs
            .into_iter()
            .map(|(pubkey, account)| {
                let data: &[u8] = &account.data;
                let acc: &Acc = from_bytes(&data[8..]);
                Ok((pubkey, *acc))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(parsed_accounts)
    }
}
