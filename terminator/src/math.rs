use anchor_lang::prelude::Pubkey;
use anyhow::Result;
use colored::Colorize;
use kamino_lending::{utils::Fraction, LiquidationParams, Obligation};
use tracing::{debug, info};

use crate::{liquidator::Holdings, model::StateWithKey};

#[derive(Debug)]
pub enum LiquidationStrategy {
    LiquidateAndRedeem(u64),
    SwapThenLiquidate(u64, u64),
}

pub struct ObligationInfo {
    pub borrowed_amount: Fraction,
    pub deposited_amount: Fraction,
    pub ltv: Fraction,
    pub unhealthy_ltv: Fraction,
}

pub fn obligation_info(address: &Pubkey, obligation: &Obligation) -> ObligationInfo {
    let borrowed_amount = Fraction::from_bits(obligation.borrow_factor_adjusted_debt_value_sf);
    let deposited_amount = Fraction::from_bits(obligation.deposited_value_sf);

    let (ltv, unhealthy_ltv) = if borrowed_amount > 0 && deposited_amount == 0 {
        info!("Obligation {address} has bad debt: {:?}", obligation);
        (Fraction::ZERO, Fraction::ZERO)
    } else if deposited_amount > 0 || borrowed_amount > 0 {
        (
            obligation.loan_to_value(),
            obligation.unhealthy_loan_to_value(),
        )
    } else {
        (Fraction::ZERO, Fraction::ZERO)
    };

    ObligationInfo {
        borrowed_amount,
        deposited_amount,
        ltv,
        unhealthy_ltv,
    }
}

pub fn print_obligation_stats(
    obl_info: &ObligationInfo,
    address: &Pubkey,
    i: usize,
    num_obligations: usize,
) {
    let ObligationInfo {
        borrowed_amount,
        deposited_amount,
        ltv,
        unhealthy_ltv,
    } = obl_info;

    let is_liquidatable = ltv > unhealthy_ltv;
    let msg = format!(
        "{}/{} obligation: {}, healthy: {}, LTV: {:?}%/{:?}%, deposited: {} borrowed: {}",
        i + 1,
        num_obligations,
        address.to_string().green(),
        if is_liquidatable {
            "NO".red()
        } else {
            "YES".green()
        },
        ltv * 100,
        unhealthy_ltv * 100,
        deposited_amount,
        borrowed_amount
    );
    if is_liquidatable {
        info!("{}", msg);
    } else {
        debug!("{}", msg);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn decide_liquidation_strategy(
    base_mint: &Pubkey,
    obligation: &StateWithKey<Obligation>,
    lending_market: &StateWithKey<kamino_lending::LendingMarket>,
    coll_reserve: &StateWithKey<kamino_lending::Reserve>,
    debt_reserve: &StateWithKey<kamino_lending::Reserve>,
    clock: &anchor_lang::prelude::Clock,
    max_allowed_ltv_override_pct_opt: Option<u64>,
    liquidation_swap_slippage_pct: f64,
    holdings: Holdings,
) -> Result<Option<LiquidationStrategy>> {
    let debt_res_key = debt_reserve.key;

    // Calculate what is possible first
    let LiquidationParams { user_ltv, .. } =
        kamino_lending::liquidation_operations::get_liquidation_params(
            &lending_market.state.borrow(),
            &coll_reserve.state.borrow(),
            &debt_reserve.state.borrow(),
            &obligation.state.borrow(),
            clock.slot,
            true, // todo actually check if this is true
            true, // todo actually check if this is true
            max_allowed_ltv_override_pct_opt,
        )?;

    let obligation_state = obligation.state.borrow();
    let lending_market = lending_market.state.borrow();
    let (debt_liquidity, _) = obligation_state
        .find_liquidity_in_borrows(debt_res_key)
        .unwrap();

    let full_debt_amount_f = Fraction::from_bits(debt_liquidity.borrowed_amount_sf);
    let full_debt_mv_f = Fraction::from_bits(debt_liquidity.market_value_sf);

    let liquidatable_debt =
        kamino_lending::liquidation_operations::max_liquidatable_borrowed_amount(
            &obligation_state,
            lending_market.liquidation_max_debt_close_factor_pct,
            lending_market.max_liquidatable_debt_market_value_at_once,
            debt_liquidity,
            user_ltv,
            lending_market.insolvency_risk_unhealthy_ltv_pct,
        )
        .min(full_debt_amount_f);

    let liquidation_ratio = liquidatable_debt / full_debt_amount_f;

    // This is what is possible, liquidatable/repayable debt in lamports and in $ terms
    let liqidatable_amount: u64 = liquidatable_debt.to_num();
    let liquidable_mv = liquidation_ratio * full_debt_mv_f;

    // Compare to what we have already
    let debt_mint = &debt_reserve.state.borrow().liquidity.mint_pubkey;

    let debt_holding = holdings.holding_of(debt_mint).unwrap();
    let base_holding = holdings.holding_of(base_mint).unwrap();

    // Always assume we are fully rebalanced otherwise it's very hard to decide
    let decision = if debt_mint == base_mint {
        // liquidate as much as we have / is possible
        let debt_holding_balance = debt_holding.balance;
        let liquidate_amount = debt_holding_balance.min(liqidatable_amount);

        info!(
            "LiquidateAndRedeem math nums
            liquidate_amount: {liquidate_amount}
            debt_holding.balance: {debt_holding_balance}
            liquidatable_amount: {liqidatable_amount},"
        );

        Some(LiquidationStrategy::LiquidateAndRedeem(liquidate_amount))
    } else {
        // Swap base token to debt token then liquidate
        // compare market values, add slippage, then swap base token + slippage into debt token
        let holding_mv = base_holding.usd_value;
        let liquidable_mv: f64 = liquidable_mv.to_num();

        // Adjust by slippage how much we would have if we had to swap
        let holding_mv = holding_mv * (1.0 - liquidation_swap_slippage_pct / 100.0);

        // Liquidate as much as possible (the lowest of holding or liquidatable in $ terms)
        let liquidation_mv = holding_mv.min(liquidable_mv);

        // Decide on the final amount
        let ratio = liquidation_mv / holding_mv;
        let swap_amount = (base_holding.balance as f64 * ratio) as u64;
        let liquidate_amount = (liqidatable_amount as f64 * ratio) as u64;

        info!(
            "SwapAndLiquidate math nums
            holding_mv: {holding_mv},
            liquidable_mv: {liquidable_mv},
            liquidation_mv: {liquidation_mv},
            ratio: {ratio},
            swap_amount: {swap_amount},
            liquidate_amount: {liquidate_amount}"
        );

        Some(LiquidationStrategy::SwapThenLiquidate(
            swap_amount,
            liquidate_amount,
        ))

        // TODO: what if we have barely anything?
        // need to do flash loans
    };

    // Print everything such that we can debug later how the decision was made
    info!(
        "Liquidation decision: {decision:?},
        full_debt_amount: {full_debt_amount_f},
        full_debt_mv: {full_debt_mv_f},
        liquidatable_debt: {liquidatable_debt},
        liquidation_ratio: {liquidation_ratio},
        liqidatable_amount: {liqidatable_amount},
        liquidable_mv: {liquidable_mv},
        debt_holding: {debt_holding:?},
        base_holding: {base_holding:?},
        liquidation_swap_slippage_pct: {liquidation_swap_slippage_pct}
    "
    );

    Ok(decision)
}
