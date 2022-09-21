use crate::controller::spot_balance::{
    update_revenue_pool_balances, update_spot_balances, update_spot_market_cumulative_interest,
};
use crate::math::spot_balance::get_token_amount;

use crate::error::ClearingHouseResult;
use crate::error::ErrorCode;
use crate::math::amm::calculate_net_user_pnl;
use crate::math::casting::{cast_to_i128, cast_to_i64, cast_to_u128, cast_to_u32, cast_to_u64};
use crate::math::constants::{
    MAX_APR_PER_REVENUE_SETTLE_PRECISION, MAX_APR_PER_REVENUE_SETTLE_TO_INSURANCE_FUND_VAULT,
    ONE_YEAR, SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_DENOMINATOR,
    SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_NUMERATOR,
};
use crate::math::helpers::get_proportion_u128;
use crate::math::insurance::{
    calculate_if_shares_lost, calculate_rebase_info, if_shares_to_vault_amount,
    vault_amount_to_if_shares,
};
use crate::math::spot_balance::validate_spot_market_amounts;
use crate::math_error;
use crate::state::events::{InsuranceFundRecord, InsuranceFundStakeRecord, StakeAction};
use crate::state::insurance_fund_stake::InsuranceFundStake;
use crate::state::market::PerpMarket;
use crate::state::spot_market::{SpotBalanceType, SpotMarket};
use crate::state::user::UserStats;
use crate::{emit, validate};
use solana_program::msg;

pub fn add_insurance_fund_stake(
    amount: u64,
    insurance_vault_amount: u64,
    insurance_fund_stake: &mut InsuranceFundStake,
    user_stats: &mut UserStats,
    spot_market: &mut SpotMarket,
    now: i64,
) -> ClearingHouseResult {
    validate!(
        !(insurance_vault_amount == 0 && spot_market.total_if_shares != 0),
        ErrorCode::DefaultError,
        "Insurance Fund balance should be non-zero for new LPs to enter"
    )?;

    apply_rebase_to_insurance_fund(insurance_vault_amount, spot_market)?;
    apply_rebase_to_insurance_fund_stake(insurance_fund_stake, spot_market)?;

    let if_shares_before = insurance_fund_stake.checked_if_shares(spot_market)?;
    let total_if_shares_before = spot_market.total_if_shares;
    let user_if_shares_before = spot_market.user_if_shares;

    let n_shares =
        vault_amount_to_if_shares(amount, spot_market.total_if_shares, insurance_vault_amount)?;

    // reset cost basis if no shares
    insurance_fund_stake.cost_basis = if if_shares_before == 0 {
        cast_to_i64(amount)?
    } else {
        insurance_fund_stake
            .cost_basis
            .checked_add(cast_to_i64(amount)?)
            .ok_or_else(math_error!())?
    };

    insurance_fund_stake.increase_if_shares(n_shares, spot_market)?;

    spot_market.total_if_shares = spot_market
        .total_if_shares
        .checked_add(n_shares)
        .ok_or_else(math_error!())?;

    spot_market.user_if_shares = spot_market
        .user_if_shares
        .checked_add(n_shares)
        .ok_or_else(math_error!())?;

    if spot_market.market_index == 0 {
        user_stats.staked_quote_asset_amount = if_shares_to_vault_amount(
            insurance_fund_stake.checked_if_shares(spot_market)?,
            spot_market.total_if_shares,
            insurance_vault_amount
                .checked_add(amount)
                .ok_or_else(math_error!())?,
        )?;
    }

    let if_shares_after = insurance_fund_stake.checked_if_shares(spot_market)?;

    emit!(InsuranceFundStakeRecord {
        ts: now,
        user_authority: user_stats.authority,
        action: StakeAction::Stake,
        amount,
        market_index: spot_market.market_index,
        insurance_vault_amount_before: insurance_vault_amount,
        if_shares_before,
        user_if_shares_before,
        total_if_shares_before,
        if_shares_after,
        total_if_shares_after: spot_market.total_if_shares,
        user_if_shares_after: spot_market.user_if_shares,
    });

    Ok(())
}

pub fn apply_rebase_to_insurance_fund(
    insurance_fund_vault_balance: u64,
    spot_market: &mut SpotMarket,
) -> ClearingHouseResult {
    if insurance_fund_vault_balance != 0
        && cast_to_u128(insurance_fund_vault_balance)? < spot_market.total_if_shares
    {
        let (expo_diff, rebase_divisor) =
            calculate_rebase_info(spot_market.total_if_shares, insurance_fund_vault_balance)?;

        spot_market.total_if_shares = spot_market
            .total_if_shares
            .checked_div(rebase_divisor)
            .ok_or_else(math_error!())?;
        spot_market.user_if_shares = spot_market
            .user_if_shares
            .checked_div(rebase_divisor)
            .ok_or_else(math_error!())?;
        spot_market.if_shares_base = spot_market
            .if_shares_base
            .checked_add(cast_to_u128(expo_diff)?)
            .ok_or_else(math_error!())?;

        msg!("rebasing insurance fund: expo_diff={}", expo_diff);
    }

    if insurance_fund_vault_balance != 0 && spot_market.total_if_shares == 0 {
        spot_market.total_if_shares = cast_to_u128(insurance_fund_vault_balance)?;
    }

    Ok(())
}

pub fn apply_rebase_to_insurance_fund_stake(
    insurance_fund_stake: &mut InsuranceFundStake,
    spot_market: &mut SpotMarket,
) -> ClearingHouseResult {
    if spot_market.if_shares_base != insurance_fund_stake.if_base {
        validate!(
            spot_market.if_shares_base > insurance_fund_stake.if_base,
            ErrorCode::DefaultError,
            "Rebase expo out of bounds"
        )?;

        let expo_diff = cast_to_u32(spot_market.if_shares_base - insurance_fund_stake.if_base)?;

        let rebase_divisor = 10_u128.pow(expo_diff);

        msg!(
            "rebasing insurance fund stake: base: {} -> {} ",
            insurance_fund_stake.if_base,
            spot_market.if_shares_base,
        );

        insurance_fund_stake.if_base = spot_market.if_shares_base;

        let old_if_shares = insurance_fund_stake.unchecked_if_shares();
        let new_if_shares = old_if_shares
            .checked_div(rebase_divisor)
            .ok_or_else(math_error!())?;

        msg!(
            "rebasing insurance fund stake: shares -> {} ",
            new_if_shares
        );

        insurance_fund_stake.update_if_shares(new_if_shares, spot_market)?;

        insurance_fund_stake.last_withdraw_request_shares = insurance_fund_stake
            .last_withdraw_request_shares
            .checked_div(rebase_divisor)
            .ok_or_else(math_error!())?;
    }

    Ok(())
}

pub fn request_remove_insurance_fund_stake(
    n_shares: u128,
    insurance_vault_amount: u64,
    insurance_fund_stake: &mut InsuranceFundStake,
    user_stats: &mut UserStats,
    spot_market: &mut SpotMarket,
    now: i64,
) -> ClearingHouseResult {
    msg!("n_shares {}", n_shares);
    insurance_fund_stake.last_withdraw_request_shares = n_shares;

    apply_rebase_to_insurance_fund(insurance_vault_amount, spot_market)?;
    apply_rebase_to_insurance_fund_stake(insurance_fund_stake, spot_market)?;

    let if_shares_before = insurance_fund_stake.checked_if_shares(spot_market)?;
    let total_if_shares_before = spot_market.total_if_shares;
    let user_if_shares_before = spot_market.user_if_shares;

    validate!(
        insurance_fund_stake.last_withdraw_request_shares
            <= insurance_fund_stake.checked_if_shares(spot_market)?,
        ErrorCode::DefaultError,
        "last_withdraw_request_shares exceeds if_shares {} > {}",
        insurance_fund_stake.last_withdraw_request_shares,
        insurance_fund_stake.checked_if_shares(spot_market)?
    )?;

    validate!(
        insurance_fund_stake.if_base == spot_market.if_shares_base,
        ErrorCode::DefaultError,
        "if stake base != spot market base"
    )?;

    insurance_fund_stake.last_withdraw_request_value = if_shares_to_vault_amount(
        insurance_fund_stake.last_withdraw_request_shares,
        spot_market.total_if_shares,
        insurance_vault_amount,
    )?
    .min(insurance_vault_amount.saturating_sub(1));

    validate!(
        insurance_fund_stake.last_withdraw_request_value == 0
            || insurance_fund_stake.last_withdraw_request_value < insurance_vault_amount,
        ErrorCode::DefaultError,
        "Requested withdraw value is not below Insurance Fund balance"
    )?;

    let if_shares_after = insurance_fund_stake.checked_if_shares(spot_market)?;

    if spot_market.market_index == 0 {
        user_stats.staked_quote_asset_amount = if_shares_to_vault_amount(
            insurance_fund_stake.checked_if_shares(spot_market)?,
            spot_market.total_if_shares,
            insurance_vault_amount,
        )?;
    }

    emit!(InsuranceFundStakeRecord {
        ts: now,
        user_authority: user_stats.authority,
        action: StakeAction::UnstakeRequest,
        amount: insurance_fund_stake.last_withdraw_request_value,
        market_index: spot_market.market_index,
        insurance_vault_amount_before: insurance_vault_amount,
        if_shares_before,
        user_if_shares_before,
        total_if_shares_before,
        if_shares_after,
        total_if_shares_after: spot_market.total_if_shares,
        user_if_shares_after: spot_market.user_if_shares,
    });

    insurance_fund_stake.last_withdraw_request_ts = now;

    Ok(())
}

pub fn cancel_request_remove_insurance_fund_stake(
    insurance_vault_amount: u64,
    insurance_fund_stake: &mut InsuranceFundStake,
    user_stats: &mut UserStats,
    spot_market: &mut SpotMarket,
    now: i64,
) -> ClearingHouseResult {
    apply_rebase_to_insurance_fund(insurance_vault_amount, spot_market)?;
    apply_rebase_to_insurance_fund_stake(insurance_fund_stake, spot_market)?;

    let if_shares_before = insurance_fund_stake.checked_if_shares(spot_market)?;
    let total_if_shares_before = spot_market.total_if_shares;
    let user_if_shares_before = spot_market.user_if_shares;

    validate!(
        insurance_fund_stake.if_base == spot_market.if_shares_base,
        ErrorCode::DefaultError,
        "if stake base != spot market base"
    )?;

    validate!(
        insurance_fund_stake.last_withdraw_request_shares != 0,
        ErrorCode::DefaultError,
        "No withdraw request in progress"
    )?;

    let if_shares_lost =
        calculate_if_shares_lost(insurance_fund_stake, spot_market, insurance_vault_amount)?;

    insurance_fund_stake.decrease_if_shares(if_shares_lost, spot_market)?;

    spot_market.total_if_shares = spot_market
        .total_if_shares
        .checked_sub(if_shares_lost)
        .ok_or_else(math_error!())?;

    spot_market.user_if_shares = spot_market
        .user_if_shares
        .checked_sub(if_shares_lost)
        .ok_or_else(math_error!())?;

    let if_shares_after = insurance_fund_stake.checked_if_shares(spot_market)?;

    if spot_market.market_index == 0 {
        user_stats.staked_quote_asset_amount = if_shares_to_vault_amount(
            if_shares_after,
            spot_market.total_if_shares,
            insurance_vault_amount,
        )?;
    }

    emit!(InsuranceFundStakeRecord {
        ts: now,
        user_authority: user_stats.authority,
        action: StakeAction::UnstakeCancelRequest,
        amount: 0,
        market_index: spot_market.market_index,
        insurance_vault_amount_before: insurance_vault_amount,
        if_shares_before,
        user_if_shares_before,
        total_if_shares_before,
        if_shares_after,
        total_if_shares_after: spot_market.total_if_shares,
        user_if_shares_after: spot_market.user_if_shares,
    });

    insurance_fund_stake.last_withdraw_request_shares = 0;
    insurance_fund_stake.last_withdraw_request_value = 0;
    insurance_fund_stake.last_withdraw_request_ts = now;

    Ok(())
}

pub fn remove_insurance_fund_stake(
    insurance_vault_amount: u64,
    insurance_fund_stake: &mut InsuranceFundStake,
    user_stats: &mut UserStats,
    spot_market: &mut SpotMarket,
    now: i64,
) -> ClearingHouseResult<u64> {
    let time_since_withdraw_request = now
        .checked_sub(insurance_fund_stake.last_withdraw_request_ts)
        .ok_or_else(math_error!())?;

    validate!(
        time_since_withdraw_request >= spot_market.insurance_withdraw_escrow_period,
        ErrorCode::TryingToRemoveLiquidityTooFast
    )?;

    apply_rebase_to_insurance_fund(insurance_vault_amount, spot_market)?;
    apply_rebase_to_insurance_fund_stake(insurance_fund_stake, spot_market)?;

    let if_shares_before = insurance_fund_stake.checked_if_shares(spot_market)?;
    let total_if_shares_before = spot_market.total_if_shares;
    let user_if_shares_before = spot_market.user_if_shares;

    let n_shares = insurance_fund_stake.last_withdraw_request_shares;

    validate!(
        n_shares > 0,
        ErrorCode::DefaultError,
        "Must submit withdraw request and wait the escrow period"
    )?;

    validate!(
        if_shares_before >= n_shares,
        ErrorCode::InsufficientLPTokens
    )?;

    let amount = if_shares_to_vault_amount(
        n_shares,
        spot_market.total_if_shares,
        insurance_vault_amount,
    )?;

    let _if_shares_lost =
        calculate_if_shares_lost(insurance_fund_stake, spot_market, insurance_vault_amount)?;

    let withdraw_amount = amount.min(insurance_fund_stake.last_withdraw_request_value);

    insurance_fund_stake.decrease_if_shares(n_shares, spot_market)?;

    insurance_fund_stake.cost_basis = insurance_fund_stake
        .cost_basis
        .checked_sub(cast_to_i64(withdraw_amount)?)
        .ok_or_else(math_error!())?;

    spot_market.total_if_shares = spot_market
        .total_if_shares
        .checked_sub(n_shares)
        .ok_or_else(math_error!())?;

    spot_market.user_if_shares = spot_market
        .user_if_shares
        .checked_sub(n_shares)
        .ok_or_else(math_error!())?;

    // reset insurance_fund_stake withdraw request info
    insurance_fund_stake.last_withdraw_request_shares = 0;
    insurance_fund_stake.last_withdraw_request_value = 0;
    insurance_fund_stake.last_withdraw_request_ts = now;

    let if_shares_after = insurance_fund_stake.checked_if_shares(spot_market)?;

    if spot_market.market_index == 0 {
        user_stats.staked_quote_asset_amount = if_shares_to_vault_amount(
            if_shares_after,
            spot_market.total_if_shares,
            insurance_vault_amount
                .checked_sub(amount)
                .ok_or_else(math_error!())?,
        )?;
    }

    emit!(InsuranceFundStakeRecord {
        ts: now,
        user_authority: user_stats.authority,
        action: StakeAction::Unstake,
        amount: withdraw_amount,
        market_index: spot_market.market_index,
        insurance_vault_amount_before: insurance_vault_amount,
        if_shares_before,
        user_if_shares_before,
        total_if_shares_before,
        if_shares_after,
        total_if_shares_after: spot_market.total_if_shares,
        user_if_shares_after: spot_market.user_if_shares,
    });

    Ok(withdraw_amount)
}

pub fn settle_revenue_to_insurance_fund(
    spot_market_vault_amount: u64,
    insurance_vault_amount: u64,
    spot_market: &mut SpotMarket,
    now: i64,
) -> ClearingHouseResult<u64> {
    update_spot_market_cumulative_interest(spot_market, now)?;

    validate!(
        spot_market.revenue_settle_period > 0,
        ErrorCode::DefaultError,
        "invalid revenue_settle_period settings on spot market"
    )?;

    validate!(
        spot_market.user_if_factor <= spot_market.total_if_factor,
        ErrorCode::DefaultError,
        "invalid if_factor settings on spot market"
    )?;

    let depositors_claim = cast_to_u128(validate_spot_market_amounts(
        spot_market,
        spot_market_vault_amount,
    )?)?;

    let mut token_amount = get_token_amount(
        spot_market.revenue_pool.balance,
        spot_market,
        &SpotBalanceType::Deposit,
    )?;

    if depositors_claim < token_amount {
        // only allow half of withdraw available when utilization is high
        token_amount = depositors_claim.checked_div(2).ok_or_else(math_error!())?;
    }

    if spot_market.user_if_shares > 0 {
        let capped_apr_amount = cast_to_u128(
            insurance_vault_amount
                .checked_mul(MAX_APR_PER_REVENUE_SETTLE_TO_INSURANCE_FUND_VAULT)
                .ok_or_else(math_error!())?
                .checked_div(MAX_APR_PER_REVENUE_SETTLE_PRECISION)
                .ok_or_else(math_error!())?
                .checked_div(cast_to_u64(ONE_YEAR)?)
                .ok_or_else(math_error!())?
                .checked_div(cast_to_u64(spot_market.revenue_settle_period)?)
                .ok_or_else(math_error!())?,
        )?;
        token_amount = token_amount.min(capped_apr_amount);
    }

    let insurance_fund_token_amount = cast_to_u64(get_proportion_u128(
        token_amount,
        SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_NUMERATOR,
        SHARE_OF_REVENUE_ALLOCATED_TO_INSURANCE_FUND_VAULT_DENOMINATOR,
    )?)?;

    validate!(
        insurance_fund_token_amount != 0,
        ErrorCode::DefaultError,
        "no amount to settle to insurance fund"
    )?;

    spot_market.last_revenue_settle_ts = now;

    let protocol_if_factor = spot_market
        .total_if_factor
        .checked_sub(spot_market.user_if_factor)
        .ok_or_else(math_error!())?;

    // give protocol its cut
    let n_shares = vault_amount_to_if_shares(
        insurance_fund_token_amount
            .checked_mul(cast_to_u64(protocol_if_factor)?)
            .ok_or_else(math_error!())?
            .checked_div(cast_to_u64(spot_market.total_if_factor)?)
            .ok_or_else(math_error!())?,
        spot_market.total_if_shares,
        insurance_vault_amount,
    )?;

    let total_if_shares_before = spot_market.total_if_shares;

    spot_market.total_if_shares = spot_market
        .total_if_shares
        .checked_add(n_shares)
        .ok_or_else(math_error!())?;

    update_revenue_pool_balances(
        cast_to_u128(insurance_fund_token_amount)?,
        &SpotBalanceType::Borrow,
        spot_market,
    )?;

    emit!(InsuranceFundRecord {
        ts: now,
        spot_market_index: spot_market.market_index,
        perp_market_index: 0, // todo: make option?
        amount: cast_to_i64(insurance_fund_token_amount)?,

        user_if_factor: spot_market.user_if_factor,
        total_if_factor: spot_market.total_if_factor,
        vault_amount_before: spot_market_vault_amount,
        insurance_vault_amount_before: insurance_vault_amount,
        total_if_shares_before,
        total_if_shares_after: spot_market.total_if_shares,
    });

    cast_to_u64(insurance_fund_token_amount)
}

pub fn resolve_perp_pnl_deficit(
    bank_vault_amount: u64,
    insurance_vault_amount: u64,
    bank: &mut SpotMarket,
    market: &mut PerpMarket,
    now: i64,
) -> ClearingHouseResult<u64> {
    validate!(
        market.amm.total_fee_minus_distributions < 0,
        ErrorCode::DefaultError,
        "market.amm.total_fee_minus_distributions={} must be negative",
        market.amm.total_fee_minus_distributions
    )?;

    let pnl_pool_token_amount =
        get_token_amount(market.pnl_pool.balance, bank, &SpotBalanceType::Deposit)?;

    validate!(
        pnl_pool_token_amount == 0,
        ErrorCode::DefaultError,
        "pnl_pool_token_amount > 0 (={})",
        pnl_pool_token_amount
    )?;

    update_spot_market_cumulative_interest(bank, now)?;

    let total_if_shares_before = bank.total_if_shares;

    let excess_user_pnl_imbalance = if market.unrealized_max_imbalance > 0 {
        let net_unsettled_pnl = calculate_net_user_pnl(&market.amm, market.amm.last_oracle_price)?;

        net_unsettled_pnl
            .checked_sub(cast_to_i128(market.unrealized_max_imbalance)?)
            .ok_or_else(math_error!())?
    } else {
        0
    };

    validate!(
        excess_user_pnl_imbalance > 0,
        ErrorCode::DefaultError,
        "No excess_user_pnl_imbalance({}) to settle",
        excess_user_pnl_imbalance
    )?;

    let max_revenue_withdraw_per_period = cast_to_i128(
        market
            .max_revenue_withdraw_per_period
            .checked_sub(market.revenue_withdraw_since_last_settle)
            .ok_or_else(math_error!())?,
    )?;
    validate!(
        max_revenue_withdraw_per_period > 0,
        ErrorCode::DefaultError,
        "max_revenue_withdraw_per_period={} as already been reached",
        max_revenue_withdraw_per_period
    )?;

    let max_insurance_withdraw = cast_to_i128(
        market
            .quote_max_insurance
            .checked_sub(market.quote_settled_insurance)
            .ok_or_else(math_error!())?,
    )?;

    validate!(
        max_insurance_withdraw > 0,
        ErrorCode::DefaultError,
        "max_insurance_withdraw={}/{} as already been reached",
        market.quote_settled_insurance,
        market.quote_max_insurance,
    )?;

    let insurance_withdraw = excess_user_pnl_imbalance
        .min(max_revenue_withdraw_per_period)
        .min(max_insurance_withdraw)
        .min(cast_to_i128(insurance_vault_amount.saturating_sub(1))?);

    validate!(
        insurance_withdraw > 0,
        ErrorCode::DefaultError,
        "No available funds for insurance_withdraw({}) for user_pnl_imbalance={}",
        insurance_withdraw,
        excess_user_pnl_imbalance
    )?;

    market.amm.total_fee_minus_distributions = market
        .amm
        .total_fee_minus_distributions
        .checked_add(insurance_withdraw)
        .ok_or_else(math_error!())?;

    market.revenue_withdraw_since_last_settle = market
        .revenue_withdraw_since_last_settle
        .checked_add(insurance_withdraw.unsigned_abs())
        .ok_or_else(math_error!())?;

    market.quote_settled_insurance = market
        .quote_settled_insurance
        .checked_add(insurance_withdraw.unsigned_abs())
        .ok_or_else(math_error!())?;

    validate!(
        market.quote_settled_insurance <= market.quote_max_insurance,
        ErrorCode::DefaultError,
        "quote_settled_insurance breached its max {}/{}",
        market.quote_settled_insurance,
        market.quote_max_insurance,
    )?;

    market.last_revenue_withdraw_ts = now;

    update_spot_balances(
        insurance_withdraw.unsigned_abs(),
        &SpotBalanceType::Deposit,
        bank,
        &mut market.pnl_pool,
        false,
    )?;

    emit!(InsuranceFundRecord {
        ts: now,
        spot_market_index: bank.market_index,
        perp_market_index: market.market_index,
        amount: -cast_to_i64(insurance_withdraw)?,
        user_if_factor: bank.user_if_factor,
        total_if_factor: bank.total_if_factor,
        vault_amount_before: bank_vault_amount,
        insurance_vault_amount_before: insurance_vault_amount,
        total_if_shares_before,
        total_if_shares_after: bank.total_if_shares,
    });

    cast_to_u64(insurance_withdraw)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::math::constants::{QUOTE_PRECISION, SPOT_CUMULATIVE_INTEREST_PRECISION};
    use crate::state::user::UserStats;
    use anchor_lang::prelude::Pubkey;

    #[test]
    pub fn basic_stake_if_test() {
        assert_eq!(0_i32.signum(), 0);
        assert_eq!(1_i32.signum(), 1);
        assert_eq!(-1_i32.signum(), -1);

        assert_eq!(0_i128.signum(), 0);
        assert_eq!(1_i128.signum(), 1);

        let mut if_balance = 0;

        let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        let mut user_stats = UserStats {
            number_of_users: 0,
            ..UserStats::default()
        };
        let amount = QUOTE_PRECISION as u64; // $1
        let mut spot_market = SpotMarket {
            deposit_balance: 0,
            cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
            insurance_withdraw_escrow_period: 0,
            ..SpotMarket::default()
        };

        add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        assert_eq!(if_stake.unchecked_if_shares(), amount as u128);
        if_balance += amount;

        // must request first
        assert!(remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0
        )
        .is_err());

        assert_eq!(if_stake.unchecked_if_shares(), amount as u128);
        assert_eq!(spot_market.total_if_shares, amount as u128);
        assert_eq!(spot_market.if_shares_base, 0);

        request_remove_insurance_fund_stake(
            if_stake.unchecked_if_shares(),
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        assert_eq!(
            if_stake.last_withdraw_request_shares,
            if_stake.unchecked_if_shares()
        );
        assert_eq!(if_stake.last_withdraw_request_value, if_balance - 1); //rounding in favor
        assert_eq!(if_stake.unchecked_if_shares(), amount as u128);
        assert_eq!(spot_market.total_if_shares, amount as u128);
        assert_eq!(spot_market.if_shares_base, 0);

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(amount_returned, amount - 1);
        if_balance -= amount_returned;

        assert_eq!(if_stake.unchecked_if_shares(), 0);
        assert_eq!(if_stake.cost_basis, 1);
        assert_eq!(if_stake.last_withdraw_request_shares, 0);
        assert_eq!(if_stake.last_withdraw_request_value, 0);
        assert_eq!(spot_market.total_if_shares, 0);
        assert_eq!(spot_market.if_shares_base, 0);
        assert_eq!(if_balance, 1);

        add_insurance_fund_stake(
            1234,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        assert_eq!(if_stake.cost_basis, 1234);
        assert_eq!(spot_market.user_if_shares, 1234);
        assert_eq!(spot_market.total_if_shares, 1235); // protocol claims the 1 balance
        assert_eq!(spot_market.if_shares_base, 0);
    }

    #[test]
    pub fn basic_seeded_stake_if_test() {
        let mut if_balance = (1000 * QUOTE_PRECISION) as u64;
        let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        let mut user_stats = UserStats {
            number_of_users: 0,
            ..UserStats::default()
        };
        let amount = QUOTE_PRECISION as u64; // $1
        let mut spot_market = SpotMarket {
            deposit_balance: 0,
            cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
            insurance_withdraw_escrow_period: 0,
            ..SpotMarket::default()
        };

        assert_eq!(spot_market.total_if_shares, 0);
        assert_eq!(spot_market.user_if_shares, 0);

        add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();

        assert_eq!(spot_market.total_if_shares, (1001 * QUOTE_PRECISION)); // seeded works
        assert_eq!(spot_market.user_if_shares, QUOTE_PRECISION);
        assert_eq!(if_stake.unchecked_if_shares(), amount as u128);
        if_balance += amount;

        // must request first
        assert!(remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0
        )
        .is_err());
        assert_eq!(if_stake.unchecked_if_shares(), amount as u128);

        request_remove_insurance_fund_stake(
            if_stake.unchecked_if_shares(),
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        assert_eq!(
            if_stake.last_withdraw_request_shares,
            if_stake.unchecked_if_shares()
        );
        assert_eq!(if_stake.last_withdraw_request_value, 1000000);

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(amount_returned, amount);
        if_balance -= amount_returned;

        assert_eq!(if_stake.unchecked_if_shares(), 0);
        assert_eq!(if_stake.cost_basis, 0);
        assert_eq!(if_stake.last_withdraw_request_shares, 0);
        assert_eq!(if_stake.last_withdraw_request_value, 0);
        assert_eq!(if_balance, 1000000000);

        add_insurance_fund_stake(
            1234,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        assert_eq!(if_stake.cost_basis, 1234);
    }

    #[test]
    pub fn gains_stake_if_test() {
        let mut if_balance = 0;
        let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        let mut user_stats = UserStats {
            number_of_users: 0,
            ..UserStats::default()
        };
        let amount = QUOTE_PRECISION as u64; // $1
        let mut spot_market = SpotMarket {
            deposit_balance: 0,
            cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
            insurance_withdraw_escrow_period: 0,
            ..SpotMarket::default()
        };

        add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        assert_eq!(if_stake.unchecked_if_shares(), amount as u128);
        if_balance += amount;

        // gains
        if_balance += amount / 19;

        let n_shares = if_stake.unchecked_if_shares();
        let expected_amount_returned = (amount + amount / 19) / 3 - 1;

        request_remove_insurance_fund_stake(
            n_shares / 3,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(amount_returned, expected_amount_returned);
        assert_eq!(if_stake.unchecked_if_shares(), n_shares * 2 / 3 + 1);
        if_balance -= amount_returned;

        request_remove_insurance_fund_stake(
            n_shares / 3,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(if_stake.unchecked_if_shares(), n_shares / 3 + 1);
        assert_eq!(amount_returned, expected_amount_returned);
        if_balance -= amount_returned;

        request_remove_insurance_fund_stake(
            1,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(amount_returned, 1);

        request_remove_insurance_fund_stake(
            n_shares / 3 - 1,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(amount_returned, expected_amount_returned + 1);

        if_balance -= amount_returned;

        assert_eq!(if_balance, 2);
    }

    #[test]
    pub fn losses_stake_if_test() {
        let mut if_balance = 0;
        let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        let mut user_stats = UserStats {
            number_of_users: 0,
            ..UserStats::default()
        };
        let amount = QUOTE_PRECISION as u64; // $1
        let mut spot_market = SpotMarket {
            deposit_balance: 0,
            cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
            insurance_withdraw_escrow_period: 0,
            ..SpotMarket::default()
        };

        add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        assert_eq!(if_stake.unchecked_if_shares(), amount as u128);
        if_balance += amount;

        // gains
        if_balance -= amount / 19;

        let n_shares = if_stake.unchecked_if_shares();
        let expected_amount_returned = (amount - amount / 19) / 3;

        request_remove_insurance_fund_stake(
            n_shares / 3,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(amount_returned, expected_amount_returned);
        assert_eq!(if_stake.unchecked_if_shares(), n_shares * 2 / 3 + 1);
        if_balance -= amount_returned;

        request_remove_insurance_fund_stake(
            n_shares / 3,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(if_stake.unchecked_if_shares(), n_shares / 3 + 1);
        assert_eq!(amount_returned, expected_amount_returned);
        if_balance -= amount_returned;

        request_remove_insurance_fund_stake(
            1,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(if_stake.unchecked_if_shares(), n_shares / 3);
        assert_eq!(amount_returned, 0);

        request_remove_insurance_fund_stake(
            n_shares / 3,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(amount_returned, expected_amount_returned + 1);
        assert_eq!(if_stake.cost_basis, 52632);
        assert_eq!(if_stake.unchecked_if_shares(), 0);

        if_balance -= amount_returned;

        assert_eq!(if_balance, 1); // todo, should be stricer w/ rounding?
    }

    #[test]
    pub fn escrow_losses_stake_if_test() {
        let mut if_balance = 0;
        let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        let mut user_stats = UserStats {
            number_of_users: 0,
            ..UserStats::default()
        };
        let amount = (QUOTE_PRECISION * 100_000) as u64; // $100k
        let mut spot_market = SpotMarket {
            deposit_balance: 0,
            cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
            insurance_withdraw_escrow_period: 60 * 60 * 24 * 7, // 7 weeks
            ..SpotMarket::default()
        };

        let now = 7842193748;

        add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        assert_eq!(if_stake.unchecked_if_shares(), amount as u128);
        if_balance += amount;

        // losses
        if_balance -= amount / 19;

        let n_shares = if_stake.unchecked_if_shares();
        let expected_amount_returned = (amount - amount / 19) / 3;

        let o = if_shares_to_vault_amount(n_shares / 3, spot_market.total_if_shares, if_balance)
            .unwrap();
        assert_eq!(if_stake.last_withdraw_request_shares, 0);

        request_remove_insurance_fund_stake(
            n_shares / 3,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            now,
        )
        .unwrap();
        assert_eq!(if_stake.last_withdraw_request_shares, 33333333333);
        assert_eq!(
            if_stake.last_withdraw_request_value,
            expected_amount_returned
        );
        assert_eq!(expected_amount_returned, o);
        assert_eq!(o, 31578947368);

        // not enough time for withdraw
        assert!(remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            now + 60 * 60 * 24,
        )
        .is_err());

        // more losses
        if_balance = if_balance - if_balance / 2;

        // appropriate time for withdraw
        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            now + 60 * 60 * 24 * 7 + 3254,
        ))
        .unwrap();
        if_balance -= amount_returned;

        // since losses occured during withdraw, worse than expected at time of request
        assert_eq!(amount_returned < (expected_amount_returned - 1), true);
        assert_eq!(amount_returned, 15_789_473_684); //15k
        assert_eq!(if_stake.unchecked_if_shares(), n_shares * 2 / 3 + 1);
        assert_eq!(if_stake.cost_basis, 84_210_526_316); //84k
        assert_eq!(if_balance, 31_578_947_369); //31k
    }

    #[test]
    pub fn escrow_gains_stake_if_test() {
        let mut if_balance = 0;
        let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        let mut user_stats = UserStats {
            number_of_users: 0,
            ..UserStats::default()
        };
        let amount = 100_000_384_939_u64; // $100k + change
        let mut spot_market = SpotMarket {
            deposit_balance: 0,
            cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
            insurance_withdraw_escrow_period: 60 * 60 * 24 * 7, // 7 weeks
            total_if_shares: 1,
            user_if_shares: 0,
            ..SpotMarket::default()
        };

        let now = 7842193748;
        assert_eq!(if_balance, 0);
        // right now other users have claim on a zero balance IF... should not give them your money here
        assert!(add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0
        )
        .is_err());

        if_balance = 1;
        add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();

        assert_eq!(if_stake.unchecked_if_shares(), amount as u128);
        if_balance += amount;
        assert_eq!(if_balance, 100000384940);

        // gains
        if_balance += amount / 13 - 1;

        assert_eq!(if_balance, 107692722242);

        let n_shares = if_stake.unchecked_if_shares();
        let expected_amount_returned =
            (if_balance as u128 * n_shares / spot_market.total_if_shares) as u64;

        let o =
            if_shares_to_vault_amount(n_shares, spot_market.total_if_shares, if_balance).unwrap();
        request_remove_insurance_fund_stake(
            n_shares,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            now,
        )
        .unwrap();
        let value_at_req = if_stake.last_withdraw_request_value;
        assert_eq!(value_at_req, 107692722240);
        assert_eq!(o, 107692722240);

        // not enough time for withdraw
        assert!(remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            now + 60 * 60 * 24,
        )
        .is_err());

        // more gains
        if_balance = if_balance + if_balance / 412;

        let ideal_amount_returned =
            (if_balance as u128 * n_shares / spot_market.total_if_shares) as u64;

        // appropriate time for withdraw
        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            now + 60 * 60 * 24 * 7 + 3254,
        ))
        .unwrap();
        assert_eq!(if_stake.last_withdraw_request_shares, 0);
        assert_eq!(if_stake.last_withdraw_request_value, 0);

        if_balance -= amount_returned;

        assert_eq!(amount_returned < ideal_amount_returned, true);
        assert_eq!(ideal_amount_returned - amount_returned, 261390102);
        assert_eq!(amount_returned, value_at_req);

        // since gains occured, not passed on to user after request
        assert_eq!(amount_returned, (expected_amount_returned));
        assert_eq!(if_stake.unchecked_if_shares(), 0);
        assert_eq!(if_balance, 261_390_104); //$261 for protocol/other stakers
    }

    #[test]
    pub fn drained_stake_if_test_rebase_on_new_add() {
        let mut if_balance = 0;
        let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        let mut user_stats = UserStats {
            number_of_users: 0,
            ..UserStats::default()
        };
        let amount = 100_000_384_939_u64; // $100k + change

        let mut spot_market = SpotMarket {
            deposit_balance: 0,
            cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
            insurance_withdraw_escrow_period: 60 * 60 * 24 * 7, // 7 weeks
            total_if_shares: 100_000 * QUOTE_PRECISION,
            user_if_shares: 80_000 * QUOTE_PRECISION,
            ..SpotMarket::default()
        };

        assert_eq!(if_balance, 0);

        let mut orig_if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        orig_if_stake
            .update_if_shares(80_000 * QUOTE_PRECISION, &spot_market)
            .unwrap();
        let mut orig_user_stats = UserStats {
            number_of_users: 0,
            staked_quote_asset_amount: 80_000 * QUOTE_PRECISION as u64,
            ..UserStats::default()
        };

        // right now other users have claim on a zero balance IF... should not give them your money here
        assert!(add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .is_err());

        assert_eq!(if_stake.unchecked_if_shares(), 0);
        assert_eq!(spot_market.total_if_shares, 100_000_000_000);
        assert_eq!(spot_market.user_if_shares, 80_000 * QUOTE_PRECISION);

        // make non-zero
        if_balance = 1;
        add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        if_balance += amount;

        // check rebase math
        assert_eq!(spot_market.total_if_shares, 1000003849400);
        assert_eq!(spot_market.user_if_shares, 1000003849398);
        assert_eq!(if_stake.unchecked_if_shares(), 1000003849390);
        assert_eq!(
            if_stake.unchecked_if_shares() < spot_market.user_if_shares,
            true
        );
        assert_eq!(
            spot_market.user_if_shares - if_stake.unchecked_if_shares(),
            8
        );

        assert_eq!(spot_market.if_shares_base, 10);
        assert_eq!(if_stake.if_base, 10);

        // check orig if stake is good (on add)
        assert_eq!(orig_if_stake.if_base, 0);
        assert_eq!(orig_if_stake.unchecked_if_shares(), 80000000000);

        let expected_shares_for_amount =
            vault_amount_to_if_shares(1, spot_market.total_if_shares, if_balance).unwrap();
        assert_eq!(expected_shares_for_amount, 10);

        add_insurance_fund_stake(
            1,
            if_balance,
            &mut orig_if_stake,
            &mut orig_user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();

        assert_eq!(spot_market.if_shares_base, 10);
        assert_eq!(orig_if_stake.if_base, 10);
        assert_eq!(
            orig_if_stake.unchecked_if_shares(),
            80000000000 / 10000000000 + expected_shares_for_amount
        );
        assert_eq!(
            orig_if_stake.unchecked_if_shares(),
            8 + expected_shares_for_amount
        );
    }

    #[test]
    pub fn drained_stake_if_test_rebase_on_old_remove_all() {
        let mut if_balance = 0;

        let mut spot_market = SpotMarket {
            deposit_balance: 0,
            cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
            insurance_withdraw_escrow_period: 0,
            total_if_shares: 100_000 * QUOTE_PRECISION,
            user_if_shares: 80_000 * QUOTE_PRECISION,
            ..SpotMarket::default()
        };

        let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        if_stake
            .update_if_shares(80_000 * QUOTE_PRECISION, &spot_market)
            .unwrap();
        let mut user_stats = UserStats {
            number_of_users: 0,
            staked_quote_asset_amount: 80_000 * QUOTE_PRECISION as u64,
            ..UserStats::default()
        };

        assert_eq!(if_balance, 0);

        // right now other users have claim on a zero balance IF... should not give them your money here
        assert_eq!(spot_market.total_if_shares, 100_000_000_000);
        assert_eq!(spot_market.user_if_shares, 80_000 * QUOTE_PRECISION);

        request_remove_insurance_fund_stake(
            if_stake.unchecked_if_shares(),
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();

        // check rebase math
        assert_eq!(amount_returned, 0);
        assert_eq!(spot_market.total_if_shares, 20000000000);
        assert_eq!(spot_market.user_if_shares, 0);

        // make non-zero
        if_balance = 1;
        //  add_insurance_fund_stake(
        //      1,
        //      if_balance,
        //      &mut if_stake,
        //      &mut user_stats,
        //      &mut spot_market,
        //      0
        //  )
        //  .unwrap();
        //  if_balance = if_balance + 1;

        //  assert_eq!(spot_market.if_shares_base, 9);
        //  assert_eq!(spot_market.total_if_shares, 40);
        //  assert_eq!(spot_market.user_if_shares, 20);

        add_insurance_fund_stake(
            10_000_000_000_000, // 10 mil
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();

        assert_eq!(spot_market.if_shares_base, 9);
        assert_eq!(spot_market.total_if_shares, 200000000000020);
        assert_eq!(spot_market.user_if_shares, 200000000000000);
        if_balance += 10_000_000_000_000;
        assert_eq!(if_balance, 10000000000001);
    }

    #[test]
    pub fn drained_stake_if_test_rebase_on_old_remove_all_2() {
        let mut if_balance = 0;

        let mut spot_market = SpotMarket {
            deposit_balance: 0,
            cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
            insurance_withdraw_escrow_period: 0,
            total_if_shares: 100_930_021_053,
            user_if_shares: 83_021 * QUOTE_PRECISION + 135723,
            ..SpotMarket::default()
        };

        let mut if_stake = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        if_stake
            .update_if_shares(80_000 * QUOTE_PRECISION, &spot_market)
            .unwrap();
        let mut user_stats = UserStats {
            number_of_users: 0,
            staked_quote_asset_amount: 80_000 * QUOTE_PRECISION as u64,
            ..UserStats::default()
        };

        assert_eq!(if_balance, 0);

        request_remove_insurance_fund_stake(
            if_stake.unchecked_if_shares() / 2,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        ))
        .unwrap();

        // check rebase math
        assert_eq!(amount_returned, 0);
        assert_eq!(spot_market.total_if_shares, 60930021053);
        assert_eq!(spot_market.user_if_shares, 43021135723);
        assert_eq!(spot_market.if_shares_base, 0);

        if_balance = QUOTE_PRECISION as u64;

        let unstake_amt = if_stake.unchecked_if_shares() / 2;
        assert_eq!(unstake_amt, 20000000000);
        assert_eq!(if_stake.last_withdraw_request_shares, 0);
        assert_eq!(if_stake.last_withdraw_request_value, 0);
        assert_eq!(if_stake.last_withdraw_request_ts, 0);

        request_remove_insurance_fund_stake(
            unstake_amt,
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            10,
        )
        .unwrap();

        // rebase occurs in request
        assert_eq!(if_stake.last_withdraw_request_shares, unstake_amt / 1000);
        // (that rebase occurs when you pass in shares you wanna unstake) :/
        assert_eq!(if_stake.unchecked_if_shares(), 40000000);
        assert_eq!(if_stake.last_withdraw_request_value, 328245);
        assert_eq!(if_stake.last_withdraw_request_ts, 10);

        assert_eq!(spot_market.total_if_shares, 60930021);
        assert_eq!(spot_market.user_if_shares, 43021135);

        assert_eq!(spot_market.if_shares_base, 3);

        let expected_amount_for_shares = if_shares_to_vault_amount(
            if_stake.unchecked_if_shares() / 2,
            spot_market.total_if_shares,
            if_balance,
        )
        .unwrap();
        assert_eq!(
            expected_amount_for_shares,
            if_stake.last_withdraw_request_value
        );

        let user_expected_amount_for_shares_before_double = if_shares_to_vault_amount(
            spot_market.user_if_shares,
            spot_market.total_if_shares,
            if_balance,
        )
        .unwrap();

        let protocol_expected_amount_for_shares_before_double = if_shares_to_vault_amount(
            spot_market.total_if_shares - spot_market.user_if_shares,
            spot_market.total_if_shares,
            if_balance,
        )
        .unwrap();

        assert_eq!(user_expected_amount_for_shares_before_double, 706_074);
        assert_eq!(protocol_expected_amount_for_shares_before_double, 293_925);
        assert_eq!(
            user_expected_amount_for_shares_before_double
                + protocol_expected_amount_for_shares_before_double,
            if_balance - 1 // ok rounding
        );

        if_balance *= 2; // double the IF vault before withdraw

        let protocol_expected_amount_for_shares_after_double = if_shares_to_vault_amount(
            spot_market.total_if_shares - spot_market.user_if_shares,
            spot_market.total_if_shares,
            if_balance,
        )
        .unwrap();

        let user_expected_amount_for_shares_after_double = if_shares_to_vault_amount(
            spot_market.user_if_shares,
            spot_market.total_if_shares,
            if_balance,
        )
        .unwrap();

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            10,
        ))
        .unwrap();

        let protocol_expected_amount_for_shares_after_user_withdraw = if_shares_to_vault_amount(
            spot_market.total_if_shares - spot_market.user_if_shares,
            spot_market.total_if_shares,
            if_balance,
        )
        .unwrap();

        // check rebase math
        assert_eq!(if_stake.unchecked_if_shares(), 20000000);
        assert_eq!(if_stake.if_base, spot_market.if_shares_base);
        assert_eq!(if_stake.last_withdraw_request_shares, 0);
        assert_eq!(if_stake.last_withdraw_request_value, 0);

        assert_eq!(amount_returned, 328245);
        assert_eq!(spot_market.total_if_shares, 40930021);
        assert_eq!(spot_market.user_if_shares, 23021135);
        assert_eq!(spot_market.if_shares_base, 3);

        assert_eq!(
            protocol_expected_amount_for_shares_after_double,
            protocol_expected_amount_for_shares_before_double * 2
        );
        assert_eq!(
            user_expected_amount_for_shares_after_double - 1,
            user_expected_amount_for_shares_before_double * 2
        );
        assert_eq!(
            user_expected_amount_for_shares_after_double
                + protocol_expected_amount_for_shares_after_double,
            if_balance - 1 // ok rounding
        );

        assert_eq!(
            protocol_expected_amount_for_shares_after_user_withdraw,
            875_097
        );
        assert_eq!(
            protocol_expected_amount_for_shares_after_user_withdraw
                > protocol_expected_amount_for_shares_after_double,
            true
        );

        add_insurance_fund_stake(
            10_000_000_000_000, // 10 mil
            if_balance,
            &mut if_stake,
            &mut user_stats,
            &mut spot_market,
            0,
        )
        .unwrap();
        if_balance += 10_000_000_000_000;

        assert_eq!(spot_market.total_if_shares, 204650145930021);
        assert_eq!(spot_market.user_if_shares, 204650128021135);
        assert_eq!(spot_market.if_shares_base, 3);
        assert_eq!(if_balance, 10000002000000);
    }

    #[test]
    pub fn multiple_if_stakes_and_rebase() {
        let mut if_balance = 0;

        let mut if_stake_1 = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        let mut user_stats_1 = UserStats {
            number_of_users: 0,
            ..UserStats::default()
        };

        let mut if_stake_2 = InsuranceFundStake::new(Pubkey::default(), 0, 0);
        let mut user_stats_2 = UserStats {
            number_of_users: 0,
            ..UserStats::default()
        };

        let amount = (QUOTE_PRECISION * 100_000) as u64; // $100k
        let mut spot_market = SpotMarket {
            deposit_balance: 0,
            cumulative_deposit_interest: 1111 * SPOT_CUMULATIVE_INTEREST_PRECISION / 1000,
            insurance_withdraw_escrow_period: 0, // none
            ..SpotMarket::default()
        };

        add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake_1,
            &mut user_stats_1,
            &mut spot_market,
            0,
        )
        .unwrap();

        if_balance = amount;

        add_insurance_fund_stake(
            amount,
            if_balance,
            &mut if_stake_2,
            &mut user_stats_2,
            &mut spot_market,
            0,
        )
        .unwrap();

        // if gets drained
        if_balance = QUOTE_PRECISION as u64;
        assert_eq!(if_stake_1.if_base, 0);
        assert_eq!(spot_market.if_shares_base, 0);

        request_remove_insurance_fund_stake(
            if_stake_1.unchecked_if_shares(),
            if_balance,
            &mut if_stake_1,
            &mut user_stats_1,
            &mut spot_market,
            0,
        )
        .unwrap();
        assert_eq!(if_stake_1.if_base, 4);
        assert_eq!(spot_market.if_shares_base, 4);

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake_1,
            &mut user_stats_1,
            &mut spot_market,
            0,
        ))
        .unwrap();
        assert_eq!(amount_returned, 500000);
        if_balance -= amount_returned;

        assert_eq!(if_stake_2.if_base, 0);
        assert_eq!(spot_market.if_shares_base, 4);
        request_remove_insurance_fund_stake(
            if_stake_2.unchecked_if_shares(),
            if_balance,
            &mut if_stake_2,
            &mut user_stats_2,
            &mut spot_market,
            0,
        )
        .unwrap();
        assert_eq!(if_stake_2.if_base, 4);
        assert_eq!(spot_market.if_shares_base, 4);
        assert_eq!(if_stake_2.if_base < spot_market.total_if_shares, true);
        assert_eq!(if_stake_2.unchecked_if_shares(), spot_market.user_if_shares);
        assert_eq!(if_balance, 500000);

        let amount_returned = (remove_insurance_fund_stake(
            if_balance,
            &mut if_stake_2,
            &mut user_stats_2,
            &mut spot_market,
            0,
        ))
        .unwrap();

        assert_eq!(amount_returned, if_balance - 1);
        if_balance -= amount_returned;

        assert_eq!(if_balance, 1);
        assert_eq!(spot_market.user_if_shares, 0);
        assert_eq!(spot_market.total_if_shares, 0);
    }
}