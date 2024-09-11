use std::sync::Arc;

use anchor_client::solana_sdk::{instruction::Instruction, pubkey::Pubkey, signature::Keypair};
use anchor_lang::{prelude::Rent, system_program::System, Id, InstructionData, ToAccountMetas};
use anchor_spl::token::Token;
use kamino_lending::{LendingMarket, Reserve, ReserveFarmKind};
use solana_sdk::{
    signer::Signer,
    sysvar::{
        SysvarId, {self},
    },
};

use crate::{consts::NULL_PUBKEY, liquidator::Liquidator, model::StateWithKey, readable, writable};

#[derive(Clone)]
pub struct InstructionBlocks {
    pub instruction: Instruction,
    pub payer: Pubkey,
    pub signers: Vec<Arc<Keypair>>,
}

#[allow(clippy::too_many_arguments)]
pub fn liquidate_obligation_and_redeem_reserve_collateral_ix(
    program_id: &Pubkey,
    lending_market: StateWithKey<LendingMarket>,
    debt_reserve: StateWithKey<Reserve>,
    coll_reserve: StateWithKey<Reserve>,
    liquidator: &Liquidator,
    obligation: Pubkey,
    liquidity_amount: u64,
    min_acceptable_received_liq_amount: u64,
    max_allowed_ltv_override_pct_opt: Option<u64>,
) -> InstructionBlocks {
    let lending_market_pubkey = lending_market.key;
    let lending_market_authority =
        kamino_lending::utils::seeds::pda::lending_market_auth(&lending_market_pubkey);

    let coll_reserve_state = coll_reserve.state.borrow();
    let coll_reserve_address = coll_reserve.key;

    let debt_reserve_state = debt_reserve.state.borrow();
    let debt_reserve_address = debt_reserve.key;

    let collateral_ctoken = coll_reserve_state.collateral.mint_pubkey;
    let collateral_token = coll_reserve_state.liquidity.mint_pubkey;
    let debt_token = debt_reserve_state.liquidity.mint_pubkey;

    let instruction = Instruction {
        program_id: *program_id,
        accounts: kamino_lending::accounts::LiquidateObligationAndRedeemReserveCollateral {
            liquidator: liquidator.wallet.pubkey(),
            lending_market: lending_market.key,
            repay_reserve: debt_reserve_address,
            repay_reserve_liquidity_supply: debt_reserve_state.liquidity.supply_vault,
            withdraw_reserve: coll_reserve_address,
            withdraw_reserve_collateral_mint: coll_reserve_state.collateral.mint_pubkey,
            withdraw_reserve_collateral_supply: coll_reserve_state.collateral.supply_vault,
            withdraw_reserve_liquidity_supply: coll_reserve_state.liquidity.supply_vault,
            withdraw_reserve_liquidity_fee_receiver: coll_reserve_state.liquidity.fee_vault,
            lending_market_authority,
            obligation,
            user_destination_collateral: *liquidator.atas.get(&collateral_ctoken).unwrap(),
            user_destination_liquidity: *liquidator.atas.get(&collateral_token).unwrap(),
            user_source_liquidity: *liquidator.atas.get(&debt_token).unwrap(),
            instruction_sysvar_account: sysvar::instructions::ID,
            repay_liquidity_token_program: Token::id(),
            repay_reserve_liquidity_mint: debt_reserve_state.liquidity.mint_pubkey,
            withdraw_reserve_liquidity_mint: coll_reserve_state.liquidity.mint_pubkey,
            collateral_token_program: Token::id(), // TODO: add Token2022
            withdraw_liquidity_token_program: Token::id(), // TODO: add Token2022
        }
        .to_account_metas(None),
        data: kamino_lending::instruction::LiquidateObligationAndRedeemReserveCollateral {
            liquidity_amount,
            min_acceptable_received_liquidity_amount: min_acceptable_received_liq_amount,
            max_allowed_ltv_override_percent: max_allowed_ltv_override_pct_opt.unwrap_or(0),
        }
        .data(),
    };

    InstructionBlocks {
        instruction,
        payer: liquidator.wallet.pubkey(),
        signers: vec![liquidator.wallet.clone()],
    }
}

pub fn refresh_reserve_ix(
    program_id: &Pubkey,
    reserve: Reserve,
    address: &Pubkey,
    payer: Arc<Keypair>,
) -> InstructionBlocks {
    let instruction = Instruction {
        program_id: *program_id,
        accounts: kamino_lending::accounts::RefreshReserve {
            lending_market: reserve.lending_market,
            reserve: *address,
            pyth_oracle: maybe_null_pk(reserve.config.token_info.pyth_configuration.price),
            switchboard_price_oracle: maybe_null_pk(
                reserve
                    .config
                    .token_info
                    .switchboard_configuration
                    .price_aggregator,
            ),
            switchboard_twap_oracle: maybe_null_pk(
                reserve
                    .config
                    .token_info
                    .switchboard_configuration
                    .twap_aggregator,
            ),
            scope_prices: maybe_null_pk(reserve.config.token_info.scope_configuration.price_feed),
        }
        .to_account_metas(None),
        data: kamino_lending::instruction::RefreshReserve.data(),
    };

    InstructionBlocks {
        instruction,
        payer: payer.pubkey(),
        signers: vec![payer.clone()],
    }
}

pub fn refresh_obligation_farm_for_reserve_ix(
    program_id: &Pubkey,
    reserve: &StateWithKey<Reserve>,
    reserve_farm_state: Pubkey,
    obligation: Pubkey,
    owner: &Arc<Keypair>,
    farms_mode: ReserveFarmKind,
) -> InstructionBlocks {
    let (user_farm_state, _) = Pubkey::find_program_address(
        &[
            farms::utils::consts::BASE_SEED_USER_STATE,
            reserve_farm_state.as_ref(),
            obligation.as_ref(),
        ],
        &farms::ID,
    );

    let reserve_state = reserve.state.borrow();
    let reserve_address = reserve.key;
    let lending_market_authority =
        kamino_lending::utils::seeds::pda::lending_market_auth(&reserve_state.lending_market);

    let accts = kamino_lending::accounts::RefreshObligationFarmsForReserve {
        crank: owner.pubkey(),
        obligation,
        lending_market: reserve_state.lending_market,
        lending_market_authority,
        reserve: reserve_address,
        obligation_farm_user_state: user_farm_state,
        reserve_farm_state,
        rent: Rent::id(),
        farms_program: farms::id(),
        system_program: System::id(),
    };

    let instruction = Instruction {
        program_id: *program_id,
        accounts: accts.to_account_metas(None),
        data: kamino_lending::instruction::RefreshObligationFarmsForReserve {
            mode: farms_mode as u8,
        }
        .data(),
    };

    InstructionBlocks {
        instruction,
        payer: owner.pubkey(),
        signers: vec![owner.clone()],
    }
}

#[allow(clippy::too_many_arguments)]
pub fn init_obligation_farm_for_reserve_ix(
    program_id: &Pubkey,
    reserve_accounts: &StateWithKey<Reserve>,
    reserve_farm_state: Pubkey,
    obligation: &Pubkey,
    owner: &Pubkey,
    payer: &Arc<Keypair>,
    mode: ReserveFarmKind,
) -> InstructionBlocks {
    let (obligation_farm_state, _user_state_bump) = Pubkey::find_program_address(
        &[
            farms::utils::consts::BASE_SEED_USER_STATE,
            reserve_farm_state.as_ref(),
            obligation.as_ref(),
        ],
        &farms::ID,
    );

    let reserve_state = reserve_accounts.state.borrow();
    let reserve_address = reserve_accounts.key;
    let lending_market_authority =
        kamino_lending::utils::seeds::pda::lending_market_auth(&reserve_state.lending_market);

    let accts = kamino_lending::accounts::InitObligationFarmsForReserve {
        payer: payer.pubkey(),
        owner: *owner,
        obligation: *obligation,
        lending_market: reserve_state.lending_market,
        lending_market_authority,
        reserve: reserve_address,
        obligation_farm: obligation_farm_state,
        reserve_farm_state,
        rent: Rent::id(),
        farms_program: farms::id(),
        system_program: System::id(),
    };

    let instruction = Instruction {
        program_id: *program_id,
        accounts: accts.to_account_metas(None),
        data: kamino_lending::instruction::InitObligationFarmsForReserve { mode: mode as u8 }
            .data(),
    };

    InstructionBlocks {
        instruction,
        payer: payer.pubkey(),
        signers: vec![payer.clone()],
    }
}

pub fn refresh_obligation_ix(
    program_id: &Pubkey,
    market: Pubkey,
    obligation: Pubkey,
    deposit_reserves: Vec<Option<Pubkey>>,
    borrow_reserves: Vec<Option<Pubkey>>,
    referrer_token_states: Vec<Option<Pubkey>>,
    payer: Arc<Keypair>,
) -> InstructionBlocks {
    let mut accounts = kamino_lending::accounts::RefreshObligation {
        obligation,
        lending_market: market,
    }
    .to_account_metas(None);

    deposit_reserves
        .iter()
        .filter(|reserve| reserve.is_some())
        .for_each(|reserve| {
            accounts.push(readable!(reserve.unwrap()));
        });

    borrow_reserves
        .iter()
        .filter(|reserve| reserve.is_some())
        .for_each(|reserve| {
            accounts.push(writable!(reserve.unwrap()));
        });

    referrer_token_states
        .iter()
        .filter(|referrer_token_state: &&Option<Pubkey>| referrer_token_state.is_some())
        .for_each(|referrer_token_state| {
            accounts.push(writable!(referrer_token_state.unwrap()));
        });

    let instruction = Instruction {
        program_id: *program_id,
        accounts,
        data: kamino_lending::instruction::RefreshObligation.data(),
    };

    InstructionBlocks {
        instruction,
        payer: payer.pubkey(),
        signers: vec![payer.clone()],
    }
}

pub fn maybe_null_pk(pubkey: Pubkey) -> Option<Pubkey> {
    if pubkey == Pubkey::default() || pubkey == NULL_PUBKEY {
        None
    } else {
        Some(pubkey)
    }
}
