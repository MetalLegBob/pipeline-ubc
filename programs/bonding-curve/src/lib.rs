//! Pipeline's universal bonding curve.
//!
//! Anyone launches a token on a curve through the router's `create_market`. Buyers buy
//! from the curve and sellers sell back to it, through Pipeline like any other market.
//! When the curve has raised its graduation threshold, trading stops and anyone can call
//! `graduate`. That moves the raised quote and the reserved tokens into a new market on
//! the AMM the creator chose, through the target's `pipeline_seed_liquidity`, at the
//! curve's final price.
//!
//! Two launch modes:
//! - **New mint:** setup creates a Token-2022 mint with built-in metadata from a fresh
//!   keypair, mints the whole supply into the curve and removes the mint authority. The
//!   curve lives at `["curve", mint]`.
//! - **Existing mint:** any SPL or Token-2022 token. The curve lives at
//!   `["curve", mint, quote_mint, sha256(init data)]` and opens once `fund_curve`
//!   deposits the supply. Leftover tokens at graduation are claimed with
//!   `claim_leftover`.
//!
//! The quote vault holds the quote raised plus two fee buckets: the creator's curve fee
//! and the optional anti-sniper launch fee, which goes into the graduated pool. The
//! token vault holds unsold curve tokens, the graduation reserve and any unclaimed
//! creator allocation. Pricing is in `pipeline-curve-math`, shared by quote and swap.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::{
    get_return_data, invoke, invoke_signed, set_return_data,
};
use anchor_lang::solana_program::system_instruction;
use anchor_spl::associated_token::{self, AssociatedToken};
use anchor_spl::token_2022::spl_token_2022::{
    self, extension::ExtensionType, instruction::AuthorityType,
};
use anchor_spl::token_2022_extensions::spl_pod::optional_keys::OptionalNonZeroPubkey;
use anchor_spl::token_2022_extensions::{
    metadata_pointer_initialize, token_metadata_initialize, token_metadata_update_authority,
    MetadataPointerInitialize, TokenMetadataInitialize, TokenMetadataUpdateAuthority,
};
use anchor_spl::token_interface::{self, Mint, TokenInterface};
use pipeline_curve_math as math;
use pipeline_strategy_interface::{self as psi, seeds as pipeline_seeds, TradeMode};

declare_id!("ETrmUegraeyB7B6fXBfANmV7w9aEwM3wUEUzFHqo5zAb");

/// Market mint order: the launched token, then the quote token.
pub const TOKEN_INDEX: u8 = 0;
pub const QUOTE_INDEX: u8 = 1;
/// The creator's curve fee is at most 5%.
pub const MAX_CURVE_FEE_BPS: u16 = 500;
/// The anti-sniper launch fee starts at most at 90%.
pub const MAX_LAUNCH_FEE_BPS: u16 = 9_000;
/// The graduation reward is a keeper tip, so like other keeper tips it is at most 1% (of
/// the graduation threshold).
pub const MAX_GRADUATION_REWARD_BPS: u16 = 100;
/// After completion, graduation into the fallback target is allowed once this long has
/// passed without a successful graduation.
pub const FALLBACK_DELAY_SECONDS: i64 = 3 * 24 * 60 * 60;
/// The playground constant-product strategy, the fallback when the creator names none.
pub const DEFAULT_FALLBACK_PROGRAM: Pubkey =
    Pubkey::from_str_const("yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx");
pub const INCINERATOR: Pubkey =
    Pubkey::from_str_const("1nc1nerator11111111111111111111111111111111");
/// Room for a graduation target's setup data (larger AMMs' parameters take about 100
/// bytes).
pub const MAX_SEED_DATA: usize = 256;
pub const MAX_NAME: usize = 32;
pub const MAX_SYMBOL: usize = 10;
/// Typical IPFS and Arweave URIs are 60–90 characters. At 128, a launch with the longest
/// name, symbol and URI still fits in one transaction paid by the creator.
pub const MAX_URI: usize = 128;
/// A price tolerance of 100% turns graduation's price check off.
pub const MAX_PRICE_TOLERANCE_BPS: u16 = 10_000;

const BPS: u64 = 10_000;
const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

#[program]
pub mod bonding_curve {
    use super::*;

    /// Called by the Pipeline router from `create_market`. `data` is a Borsh-encoded
    /// [`InitParams`].
    pub fn pipeline_init_market<'info>(
        ctx: Context<'info, InitMarket<'info>>,
        data: Vec<u8>,
    ) -> Result<()> {
        let params = InitParams::try_from_slice(&data).map_err(|_| CurveError::BadInitData)?;
        params.validate()?;
        let accounts = &ctx.accounts;
        let curve_key = accounts.curve.key();
        let market_signer = pipeline_market_signer(&curve_key);
        require_keys_eq!(
            accounts.market_signer.key(),
            market_signer,
            CurveError::NotPipeline
        );
        let new_mint = matches!(params.launch, Launch::NewMint { .. });
        let token_mint = accounts.token_mint.key();
        let params_hash = if new_mint {
            [0u8; 32]
        } else {
            solana_sha256_hasher::hashv(&[&data]).to_bytes()
        };
        let quote_mint = accounts.quote_mint.key();
        let (expected, bump) = curve_address(&token_mint, &quote_mint, new_mint, &params_hash);
        require_keys_eq!(curve_key, expected, CurveError::WrongAccount);

        // The creator's split decides the curve, so the pool opens at its final price.
        let shape = math::solve_split(
            params.total_supply,
            params.sold_bps,
            params.pool_bps,
            params.reserve_bps,
            params.graduation_threshold,
        )
        .map_err(|_| CurveError::BadSplit)?;
        // A curve that can complete must be able to graduate: at completion the pool
        // gets at least `threshold − reward` quote and `graduation + leftover` tokens, at
        // the lowest final price (everything sold). Otherwise the raise would be locked.
        let completed = math::Curve {
            virtual_quote: shape.virtual_quote,
            virtual_tokens: shape.virtual_tokens,
            curve_tokens: shape.curve_tokens,
            threshold: params.graduation_threshold,
            sold: shape.curve_tokens,
            raised: params.graduation_threshold,
        };
        completed
            .graduation_amounts(
                params.graduation_threshold - params.graduation_reward,
                shape.graduation_tokens + shape.leftover,
            )
            .ok_or(CurveError::BadSplit)?;

        // The curve account.
        let bump_seed = [bump];
        let curve_seeds =
            curve_signer_seeds(&token_mint, &quote_mint, new_mint, &params_hash, &bump_seed);
        create_pda_account(
            &accounts.payer.to_account_info(),
            &accounts.curve.to_account_info(),
            8 + Curve::INIT_SPACE,
            &crate::ID,
            &curve_seeds,
        )?;

        // The token.
        match &params.launch {
            Launch::NewMint {
                decimals,
                name,
                symbol,
                uri,
                metadata_authority,
            } => {
                require_keys_eq!(
                    accounts.token_program.key(),
                    spl_token_2022::ID,
                    CurveError::WrongAccount
                );
                create_new_mint(
                    accounts,
                    &curve_seeds,
                    *decimals,
                    name,
                    symbol,
                    uri,
                    *metadata_authority,
                )?;
            }
            Launch::ExistingMint { .. } => {
                require_keys_eq!(
                    *accounts.token_mint.owner,
                    accounts.token_program.key(),
                    CurveError::WrongAccount
                );
                // Fails unless the account is at least mint-sized.
                mint_decimals(&accounts.token_mint)?;
            }
        }
        require_keys_eq!(
            *accounts.quote_mint.to_account_info().owner,
            accounts.quote_token_program.key(),
            CurveError::WrongAccount
        );
        require_keys_neq!(
            token_mint,
            accounts.quote_mint.key(),
            CurveError::BadInitData
        );

        for (vault, mint, program) in [
            (
                &accounts.token_vault,
                accounts.token_mint.to_account_info(),
                accounts.token_program.to_account_info(),
            ),
            (
                &accounts.quote_vault,
                accounts.quote_mint.to_account_info(),
                accounts.quote_token_program.to_account_info(),
            ),
        ] {
            associated_token::create_idempotent(CpiContext::new(
                accounts.associated_token_program.key(),
                associated_token::Create {
                    payer: accounts.payer.to_account_info(),
                    associated_token: vault.to_account_info(),
                    authority: accounts.curve.to_account_info(),
                    mint,
                    system_program: accounts.system_program.to_account_info(),
                    token_program: program,
                },
            ))?;
        }

        let slot = Clock::get()?.slot;
        let status = if new_mint {
            token_interface::mint_to(
                CpiContext::new_with_signer(
                    accounts.token_program.key(),
                    token_interface::MintTo {
                        mint: accounts.token_mint.to_account_info(),
                        to: accounts.token_vault.to_account_info(),
                        authority: accounts.curve.to_account_info(),
                    },
                    &[&curve_seeds[..]],
                ),
                params.total_supply,
            )?;
            token_interface::set_authority(
                CpiContext::new_with_signer(
                    accounts.token_program.key(),
                    token_interface::SetAuthority {
                        current_authority: accounts.curve.to_account_info(),
                        account_or_mint: accounts.token_mint.to_account_info(),
                    },
                    &[&curve_seeds[..]],
                ),
                AuthorityType::MintTokens,
                None,
            )?;
            Status::Trading
        } else {
            Status::AwaitingFunds
        };

        let curve_key_for_lp = curve_key;
        let curve = Curve {
            status,
            new_mint,
            params_hash,
            token_mint,
            quote_mint: accounts.quote_mint.key(),
            token_vault: accounts.token_vault.key(),
            quote_vault: accounts.quote_vault.key(),
            token_program: accounts.token_program.key(),
            quote_program: accounts.quote_token_program.key(),
            market_signer,
            virtual_quote: shape.virtual_quote,
            virtual_tokens: shape.virtual_tokens,
            curve_tokens: shape.curve_tokens,
            graduation_tokens: shape.graduation_tokens,
            reserve_tokens: shape.reserve_tokens,
            threshold: params.graduation_threshold,
            sold: 0,
            raised: 0,
            total_supply: params.total_supply,
            creator_fees: 0,
            launch_fees: 0,
            token_reserve: token_balance(&accounts.token_vault)?,
            quote_reserve: token_balance(&accounts.quote_vault)?,
            curve_fee_bps: params.curve_fee_bps,
            fee_recipient: params.fee_recipient,
            allocation_owner: params.allocation_owner,
            allocation_total: shape.allocation,
            allocation_claimed: 0,
            vest_seconds: params.vest_seconds,
            launch_fee_bps: params.launch_fee_bps,
            launch_fee_slots: params.launch_fee_slots,
            trading_delay_slots: params.trading_delay_slots,
            start_slot: slot,
            trading_opens_slot: slot.saturating_add(params.trading_delay_slots),
            target_program: params.target_program,
            target_seed_data: params.target_seed_data.clone(),
            fallback_program: params.fallback_program.unwrap_or(DEFAULT_FALLBACK_PROGRAM),
            fallback_seed_data: params.fallback_seed_data.clone(),
            lp_recipient: match params.lp_policy {
                LpPolicy::Locked => lp_lock_address(&curve_key_for_lp),
                LpPolicy::Burn => INCINERATOR,
                LpPolicy::Creator(owner) => owner,
            },
            leftover_recipient: match params.launch {
                Launch::ExistingMint { leftover_recipient } => leftover_recipient,
                Launch::NewMint { .. } => Pubkey::default(),
            },
            leftover_owed: 0,
            graduation_reward: params.graduation_reward,
            price_tolerance_bps: params.price_tolerance_bps,
            completed_at: 0,
            graduated_at: 0,
            graduated_program: Pubkey::default(),
            graduated_market_state: Pubkey::default(),
            bump,
        };
        let curve_info = accounts.curve.to_account_info();
        let mut account_data = curve_info.try_borrow_mut_data()?;
        let mut writer: &mut [u8] = &mut account_data;
        curve.try_serialize(&mut writer)?;
        Ok(())
    }

    pub fn pipeline_market_info(ctx: Context<MarketInfoAccounts>) -> Result<()> {
        let curve = &ctx.accounts.curve;
        let info = psi::MarketInfo {
            mints: vec![curve.token_mint.to_bytes(), curve.quote_mint.to_bytes()],
            vaults: vec![curve.token_vault.to_bytes(), curve.quote_vault.to_bytes()],
            supports_exact_out: true,
        };
        set_return_data(&borsh_encode(&info)?);
        Ok(())
    }

    /// The only strategy-specific account a swap needs is the output vault.
    pub fn pipeline_resolve_accounts(
        ctx: Context<ReadTrade>,
        request: psi::ResolveRequest,
    ) -> Result<()> {
        let (_, output_vault) = ctx.accounts.curve.vaults(&request.trade)?;
        let result = psi::ResolveResult {
            accounts: vec![psi::AccountRef {
                address: output_vault.to_bytes(),
                writable: true,
            }],
            complete: true,
        };
        set_return_data(&borsh_encode(&result)?);
        Ok(())
    }

    pub fn pipeline_quote(ctx: Context<ReadTrade>, request: psi::TradeRequest) -> Result<()> {
        let fill = ctx
            .accounts
            .curve
            .fill(&request, request.amount, Clock::get()?.slot)?;
        let quote = psi::QuoteResult {
            amount_in: fill.amount_in,
            amount_out: fill.amount_out,
        };
        set_return_data(&borsh_encode(&quote)?);
        Ok(())
    }

    /// Remaining accounts: the output vault, then any transfer-hook accounts.
    pub fn pipeline_swap<'info>(
        ctx: Context<'info, SwapAccounts<'info>>,
        request: psi::SwapRequest,
    ) -> Result<()> {
        let curve = &ctx.accounts.curve;
        require_keys_eq!(
            ctx.accounts.market_signer.key(),
            curve.market_signer,
            CurveError::NotPipeline
        );
        let trade = &request.trade;
        let (input_vault, output_vault) = curve.vaults(trade)?;
        require_keys_eq!(
            ctx.accounts.input_vault.key(),
            input_vault,
            CurveError::WrongAccount
        );
        let output_vault_info = ctx
            .remaining_accounts
            .first()
            .ok_or(CurveError::WrongAccount)?;
        require_keys_eq!(
            output_vault_info.key(),
            output_vault,
            CurveError::WrongAccount
        );
        let hook_accounts = &ctx.remaining_accounts[1..];

        let buying = trade.input_index == QUOTE_INDEX;
        let reserve_in = if buying {
            curve.quote_reserve
        } else {
            curve.token_reserve
        };
        let vault_in_balance = token_balance(&ctx.accounts.input_vault)?;
        let received = psi::credited_input(vault_in_balance, reserve_in, request.amount_sent);
        let clock = Clock::get()?;
        let amount = match trade.mode {
            TradeMode::ExactIn => received,
            TradeMode::ExactOut => trade.amount,
        };
        let fill = curve.fill(trade, amount, clock.slot)?;
        require!(fill.amount_in <= received, CurveError::InsufficientInput);
        let refund = received - fill.amount_in;
        let launch_bps = curve.launch_fee_now(clock.slot);
        let launch_share = launch_fee_share(fill.fee, curve.curve_fee_bps, launch_bps);

        let bump = [curve.bump];
        let seeds = curve.signer_seeds(&bump);
        let curve_info = ctx.accounts.curve.to_account_info();
        if refund > 0 {
            transfer_checked(
                &ctx.accounts.input_token_program,
                &ctx.accounts.input_vault,
                &ctx.accounts.input_mint,
                &ctx.accounts.trader_input,
                &curve_info,
                hook_accounts,
                refund,
                &[&seeds[..]],
            )?;
        }
        if fill.amount_out > 0 {
            transfer_checked(
                &ctx.accounts.output_token_program,
                output_vault_info,
                &ctx.accounts.output_mint,
                &ctx.accounts.trader_output,
                &curve_info,
                hook_accounts,
                fill.amount_out,
                &[&seeds[..]],
            )?;
        }

        let input_balance = token_balance(&ctx.accounts.input_vault)?;
        let output_balance = token_balance(output_vault_info)?;
        let curve = &mut ctx.accounts.curve;
        curve.sold = fill.after.sold;
        curve.raised = fill.after.raised;
        curve.launch_fees += launch_share;
        curve.creator_fees += fill.fee - launch_share;
        if buying {
            curve.quote_reserve = input_balance;
            curve.token_reserve = output_balance;
        } else {
            curve.token_reserve = input_balance;
            curve.quote_reserve = output_balance;
        }
        if fill.after.is_complete() {
            curve.status = Status::Complete;
            curve.completed_at = clock.unix_timestamp;
        }
        Ok(())
    }

    /// Custom action for existing-mint launches: deposits tokens into the curve. The
    /// curve opens once its vault holds the whole supply. Remaining accounts are
    /// transfer-hook accounts.
    pub fn fund_curve<'info>(ctx: Context<'info, FundCurve<'info>>, amount: u64) -> Result<()> {
        require!(
            ctx.accounts.curve.status == Status::AwaitingFunds,
            CurveError::WrongStatus
        );
        transfer_checked(
            &ctx.accounts.token_program,
            &ctx.accounts.funder_token,
            &ctx.accounts.token_mint,
            &ctx.accounts.token_vault,
            &ctx.accounts.funder.to_account_info(),
            ctx.remaining_accounts,
            amount,
            &[],
        )?;
        let balance = token_balance(&ctx.accounts.token_vault)?;
        let slot = Clock::get()?.slot;
        let curve = &mut ctx.accounts.curve;
        curve.token_reserve = balance;
        if balance >= curve.total_supply {
            curve.status = Status::Trading;
            curve.start_slot = slot;
            curve.trading_opens_slot = slot.saturating_add(curve.trading_delay_slots);
        }
        Ok(())
    }

    /// Custom action: the fee recipient withdraws the curve fees collected so far.
    /// Remaining accounts are transfer-hook accounts.
    pub fn claim_creator_fees<'info>(ctx: Context<'info, ClaimCreatorFees<'info>>) -> Result<()> {
        let amount = ctx.accounts.curve.creator_fees;
        if amount > 0 {
            let curve = &ctx.accounts.curve;
            let bump = [curve.bump];
            let seeds = curve.signer_seeds(&bump);
            transfer_checked(
                &ctx.accounts.quote_token_program,
                &ctx.accounts.quote_vault,
                &ctx.accounts.quote_mint,
                &ctx.accounts.destination,
                &ctx.accounts.curve.to_account_info(),
                ctx.remaining_accounts,
                amount,
                &[&seeds[..]],
            )?;
        }
        let balance = token_balance(&ctx.accounts.quote_vault)?;
        let curve = &mut ctx.accounts.curve;
        curve.creator_fees = 0;
        curve.quote_reserve = balance;
        Ok(())
    }

    /// Custom action: the fee recipient names a new fee recipient.
    pub fn set_fee_recipient(ctx: Context<SetFeeRecipient>, new_recipient: Pubkey) -> Result<()> {
        ctx.accounts.curve.fee_recipient = new_recipient;
        Ok(())
    }

    /// Custom action: the allocation owner claims the creator allocation unlocked so far.
    /// Without vesting it is claimable at once; with vesting it unlocks linearly from
    /// graduation. Remaining accounts are transfer-hook accounts.
    pub fn claim_allocation<'info>(ctx: Context<'info, ClaimAllocation<'info>>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let amount = ctx.accounts.curve.claimable_allocation(now);
        if amount > 0 {
            let curve = &ctx.accounts.curve;
            let bump = [curve.bump];
            let seeds = curve.signer_seeds(&bump);
            transfer_checked(
                &ctx.accounts.token_program,
                &ctx.accounts.token_vault,
                &ctx.accounts.token_mint,
                &ctx.accounts.destination,
                &ctx.accounts.curve.to_account_info(),
                ctx.remaining_accounts,
                amount,
                &[&seeds[..]],
            )?;
        }
        let balance = token_balance(&ctx.accounts.token_vault)?;
        let curve = &mut ctx.accounts.curve;
        curve.allocation_claimed += amount;
        curve.token_reserve = balance;
        Ok(())
    }

    /// Custom action: the leftover recipient of an existing-mint launch claims the
    /// tokens left over at graduation. Remaining accounts are transfer-hook accounts.
    pub fn claim_leftover<'info>(ctx: Context<'info, ClaimLeftover<'info>>) -> Result<()> {
        let amount = ctx.accounts.curve.leftover_owed;
        if amount > 0 {
            let curve = &ctx.accounts.curve;
            let bump = [curve.bump];
            let seeds = curve.signer_seeds(&bump);
            transfer_checked(
                &ctx.accounts.token_program,
                &ctx.accounts.token_vault,
                &ctx.accounts.token_mint,
                &ctx.accounts.destination,
                &ctx.accounts.curve.to_account_info(),
                ctx.remaining_accounts,
                amount,
                &[&seeds[..]],
            )?;
        }
        let balance = token_balance(&ctx.accounts.token_vault)?;
        let curve = &mut ctx.accounts.curve;
        curve.leftover_owed = 0;
        curve.token_reserve = balance;
        Ok(())
    }

    /// Custom action, permissionless once the curve is complete: seeds a new market on
    /// the target strategy with the raised quote (less creator fees and the caller's
    /// reward, plus any launch fees) and the tokens worth that much at the final price.
    ///
    /// Only two escrows owned by `["seed", curve]` and a rent payer `["rent", curve]`
    /// funded with at most `max_rent_lamports` from the caller are exposed to the target.
    /// Leftover tokens are burned (new mints) or owed to the leftover recipient, who claims
    /// them with `claim_leftover`.
    ///
    /// Unless the curve's price tolerance is 100%, the new market must then quote a small
    /// buy and a small sell whose mid price is within the tolerance of the price the
    /// curve offered it, or the whole graduation reverts.
    ///
    /// Remaining accounts, in order:
    /// 1. the target's own seed accounts, ending with `hook_accounts` transfer-hook
    ///    accounts, which are also used for this program's own transfers;
    /// 2. `buy_quote_accounts` accounts for the target's quote of a buy (quote in, token
    ///    out) on the new market: the fixed trade accounts
    ///    (`pipeline_strategy_interface::trade_accounts`), then whatever the target's
    ///    `pipeline_resolve_accounts` lists for that trade;
    /// 3. `sell_quote_accounts` accounts for a sell (token in, quote out), the same way.
    pub fn graduate<'info>(
        ctx: Context<'info, Graduate<'info>>,
        use_fallback: bool,
        max_rent_lamports: u64,
        hook_accounts: u8,
        buy_quote_accounts: u8,
        sell_quote_accounts: u8,
    ) -> Result<()> {
        let clock = Clock::get()?;
        let accounts = &ctx.accounts;
        let curve = &accounts.curve;
        require!(curve.status == Status::Complete, CurveError::WrongStatus);
        let (target, seed_data) = if use_fallback {
            require!(
                clock.unix_timestamp >= curve.completed_at + FALLBACK_DELAY_SECONDS,
                CurveError::FallbackTooEarly
            );
            (curve.fallback_program, curve.fallback_seed_data.clone())
        } else {
            (curve.target_program, curve.target_seed_data.clone())
        };
        require_keys_eq!(
            accounts.target_program.key(),
            target,
            CurveError::WrongAccount
        );
        require!(accounts.target_program.executable, CurveError::WrongAccount);
        let pipeline = Pubkey::new_from_array(psi::PIPELINE_PROGRAM_ID);
        let registration =
            Pubkey::find_program_address(&[pipeline_seeds::STRATEGY, target.as_ref()], &pipeline).0;
        require_keys_eq!(
            accounts.target_registration.key(),
            registration,
            CurveError::TargetNotRegistered
        );
        require_keys_eq!(
            *accounts.target_registration.owner,
            pipeline,
            CurveError::TargetNotRegistered
        );
        require_keys_eq!(
            accounts.lp_recipient.key(),
            curve.lp_recipient,
            CurveError::WrongAccount
        );
        let quote_count = buy_quote_accounts as usize + sell_quote_accounts as usize;
        let seed_count = ctx
            .remaining_accounts
            .len()
            .checked_sub(quote_count)
            .ok_or(CurveError::WrongAccount)?;
        let (seed_accounts, quote_accounts) = ctx.remaining_accounts.split_at(seed_count);
        let (buy_accounts, sell_accounts) = quote_accounts.split_at(buy_quote_accounts as usize);
        let hook_count = hook_accounts as usize;
        require!(hook_count <= seed_count, CurveError::WrongAccount);
        let hooks = &seed_accounts[seed_count - hook_count..];

        let quote_available = token_balance(&accounts.quote_vault)?
            .checked_sub(curve.creator_fees)
            .and_then(|q| q.checked_sub(curve.graduation_reward))
            .ok_or(CurveError::NothingToGraduate)?;
        let allocation_left = curve.allocation_total - curve.allocation_claimed;
        // The target's reserve share is offered on top of the pool's tokens (not to the
        // fallback, for which it stays behind as leftovers), and never prices the pool.
        let reserve = if use_fallback {
            0
        } else {
            curve.reserve_tokens
        };
        let tokens_available = token_balance(&accounts.token_vault)?
            .checked_sub(allocation_left)
            .and_then(|t| t.checked_sub(curve.reserve_tokens))
            .ok_or(CurveError::NothingToGraduate)?;
        let (pool_quote, pool_tokens) = curve
            .math()
            .graduation_amounts(quote_available, tokens_available)
            .ok_or(CurveError::NothingToGraduate)?;

        let curve_key = curve.key();
        let bump = [curve.bump];
        let curve_seeds = curve.signer_seeds(&bump);
        let (_, seed_bump) =
            Pubkey::find_program_address(&[b"seed", curve_key.as_ref()], &crate::ID);
        let (_, rent_bump) =
            Pubkey::find_program_address(&[b"rent", curve_key.as_ref()], &crate::ID);
        let seed_signer: &[&[u8]] = &[b"seed", curve_key.as_ref(), &[seed_bump]];
        let rent_signer: &[&[u8]] = &[b"rent", curve_key.as_ref(), &[rent_bump]];
        let curve_info = accounts.curve.to_account_info();

        // Move the seed amounts into the escrows. The target is offered what actually
        // arrived, which is less than what was sent for tokens with a transfer fee.
        let mut seeded = [0u64; 2];
        for (i, (escrow, mint, program, vault, amount)) in [
            (
                &accounts.token_escrow,
                &accounts.token_mint,
                &accounts.token_program,
                &accounts.token_vault,
                pool_tokens,
            ),
            (
                &accounts.quote_escrow,
                &accounts.quote_mint,
                &accounts.quote_token_program,
                &accounts.quote_vault,
                pool_quote,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            associated_token::create_idempotent(CpiContext::new(
                accounts.associated_token_program.key(),
                associated_token::Create {
                    payer: accounts.caller.to_account_info(),
                    associated_token: escrow.to_account_info(),
                    authority: accounts.seed_authority.to_account_info(),
                    mint: mint.to_account_info(),
                    system_program: accounts.system_program.to_account_info(),
                    token_program: program.to_account_info(),
                },
            ))?;
            let before = token_balance(escrow)?;
            transfer_checked(
                program,
                vault,
                mint,
                escrow,
                &curve_info,
                hooks,
                amount,
                &[&curve_seeds[..]],
            )?;
            seeded[i] = token_balance(escrow)? - before;
        }
        require!(
            seeded[0] > 0 && seeded[1] > 0,
            CurveError::NothingToGraduate
        );
        // The reserve share goes into the same escrow in its own transfer, so what arrived
        // for the pool (which prices it) is measured on its own.
        let mut reserve_seeded = 0;
        if reserve > 0 {
            let before = token_balance(&accounts.token_escrow)?;
            transfer_checked(
                &accounts.token_program,
                &accounts.token_vault,
                &accounts.token_mint,
                &accounts.token_escrow,
                &curve_info,
                hooks,
                reserve,
                &[&curve_seeds[..]],
            )?;
            reserve_seeded = token_balance(&accounts.token_escrow)? - before;
        }
        let token_escrow_before = token_balance(&accounts.token_escrow)?;
        let quote_escrow_before = token_balance(&accounts.quote_escrow)?;

        // The target must create the market state now, not point at one that already
        // exists (lamports sent to the address in advance are fine).
        require!(
            *accounts.new_market_state.owner == SYSTEM_PROGRAM_ID
                && accounts.new_market_state.data_is_empty(),
            CurveError::WrongAccount
        );

        // Fund the rent payer with the caller's bounded SOL.
        if max_rent_lamports > 0 {
            invoke_signed(
                &system_instruction::transfer(
                    accounts.caller.key,
                    accounts.rent_payer.key,
                    max_rent_lamports,
                ),
                &[
                    accounts.caller.to_account_info(),
                    accounts.rent_payer.to_account_info(),
                ],
                &[],
            )?;
        }

        // Call the target's `pipeline_seed_liquidity`.
        let mut metas = vec![
            AccountMeta::new_readonly(accounts.seed_authority.key(), true),
            AccountMeta::new(accounts.new_market_state.key(), false),
            AccountMeta::new(accounts.rent_payer.key(), true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(accounts.lp_recipient.key(), false),
            AccountMeta::new_readonly(accounts.token_mint.key(), false),
            AccountMeta::new(accounts.token_escrow.key(), false),
            AccountMeta::new_readonly(accounts.token_program.key(), false),
            AccountMeta::new_readonly(accounts.quote_mint.key(), false),
            AccountMeta::new(accounts.quote_escrow.key(), false),
            AccountMeta::new_readonly(accounts.quote_token_program.key(), false),
        ];
        let mut infos = vec![
            accounts.seed_authority.to_account_info(),
            accounts.new_market_state.to_account_info(),
            accounts.rent_payer.to_account_info(),
            accounts.system_program.to_account_info(),
            accounts.lp_recipient.to_account_info(),
            accounts.token_mint.to_account_info(),
            accounts.token_escrow.to_account_info(),
            accounts.token_program.to_account_info(),
            accounts.quote_mint.to_account_info(),
            accounts.quote_escrow.to_account_info(),
            accounts.quote_token_program.to_account_info(),
        ];
        for account in seed_accounts {
            metas.push(if account.is_writable {
                AccountMeta::new(account.key(), false)
            } else {
                AccountMeta::new_readonly(account.key(), false)
            });
            infos.push(account.clone());
        }
        infos.push(accounts.target_program.to_account_info());
        // The target is offered the pool's tokens and the reserve's together; its own
        // seed data says how many are for its reserve.
        let request = psi::SeedRequest {
            amounts: vec![seeded[0] + reserve_seeded, seeded[1]],
            data: seed_data,
        };
        invoke_signed(
            &Instruction {
                program_id: target,
                accounts: metas,
                data: psi::instruction_data(psi::discriminator::SEED_LIQUIDITY, &request),
            },
            &infos,
            &[seed_signer, rent_signer],
        )?;

        // The target must have created the market and left the rent payer a plain
        // system account.
        require_keys_eq!(
            *accounts.new_market_state.owner,
            target,
            CurveError::TargetFailed
        );
        require_keys_eq!(
            *accounts.rent_payer.owner,
            SYSTEM_PROGRAM_ID,
            CurveError::TargetFailed
        );
        require!(
            accounts.rent_payer.data_is_empty(),
            CurveError::TargetFailed
        );
        let token_taken = token_escrow_before - token_balance(&accounts.token_escrow)?;
        let quote_taken = quote_escrow_before - token_balance(&accounts.quote_escrow)?;
        require!(token_taken > 0 && quote_taken > 0, CurveError::TargetFailed);

        // The new market must open at the price it was offered, by its own quotes. This
        // catches a target that opens off-price, takes less of one side than offered or
        // prices buys and sells asymmetrically, whatever its code.
        if curve.price_tolerance_bps < MAX_PRICE_TOLERANCE_BPS {
            let new_market = NewMarket {
                target: &accounts.target_program,
                market_signer: pipeline_market_signer_for(
                    &target,
                    &accounts.new_market_state.key(),
                ),
                market_state: accounts.new_market_state.key(),
                token_mint: curve.token_mint,
                quote_mint: curve.quote_mint,
                token_program: curve.token_program,
                quote_program: curve.quote_program,
            };
            let buy = new_market.quote(buy_accounts, true, math::probe_amount(seeded[1]))?;
            let sell = new_market.quote(sell_accounts, false, math::probe_amount(seeded[0]))?;
            let probes = math::Probes {
                buy_quote_in: buy.amount_in,
                buy_tokens_out: buy.amount_out,
                sell_tokens_in: sell.amount_in,
                sell_quote_out: sell.amount_out,
            };
            require!(
                math::mid_price_within(seeded[1], seeded[0], &probes, curve.price_tolerance_bps),
                CurveError::PoolOffPrice
            );
        }

        // Return whatever the target left in the escrows.
        let token_left = token_balance(&accounts.token_escrow)?;
        let quote_left = token_balance(&accounts.quote_escrow)?;
        let quote_vault_before_return = token_balance(&accounts.quote_vault)?;
        for (escrow, mint, program, vault, amount) in [
            (
                &accounts.token_escrow,
                &accounts.token_mint,
                &accounts.token_program,
                &accounts.token_vault,
                token_left,
            ),
            (
                &accounts.quote_escrow,
                &accounts.quote_mint,
                &accounts.quote_token_program,
                &accounts.quote_vault,
                quote_left,
            ),
        ] {
            if amount > 0 {
                transfer_checked(
                    program,
                    escrow,
                    mint,
                    vault,
                    &accounts.seed_authority.to_account_info(),
                    hooks,
                    amount,
                    &[seed_signer],
                )?;
            }
        }

        // Measured, because tokens with a transfer fee arrive short.
        let quote_returned = token_balance(&accounts.quote_vault)? - quote_vault_before_return;

        // Pay the caller's reward.
        if curve.graduation_reward > 0 {
            transfer_checked(
                &accounts.quote_token_program,
                &accounts.quote_vault,
                &accounts.quote_mint,
                &accounts.reward_account,
                &curve_info,
                hooks,
                curve.graduation_reward,
                &[&curve_seeds[..]],
            )?;
        }

        // Tokens neither seeded nor owed to the allocation owner.
        let leftover = token_balance(&accounts.token_vault)? - allocation_left;
        let mut leftover_owed = 0;
        if leftover > 0 {
            if curve.new_mint {
                token_interface::burn(
                    CpiContext::new_with_signer(
                        accounts.token_program.key(),
                        token_interface::Burn {
                            mint: accounts.token_mint.to_account_info(),
                            from: accounts.token_vault.to_account_info(),
                            authority: curve_info.clone(),
                        },
                        &[&curve_seeds[..]],
                    ),
                    leftover,
                )?;
            } else {
                // Owed, not pushed: the recipient claims it with `claim_leftover`, so
                // nothing the recipient does to its own accounts can block graduation.
                leftover_owed = leftover;
            }
        }

        // Return the rent payer's unspent SOL.
        let unspent = accounts.rent_payer.lamports();
        if unspent > 0 {
            invoke_signed(
                &system_instruction::transfer(
                    accounts.rent_payer.key,
                    accounts.caller.key,
                    unspent,
                ),
                &[
                    accounts.rent_payer.to_account_info(),
                    accounts.caller.to_account_info(),
                ],
                &[rent_signer],
            )?;
        }

        let token_balance_after = token_balance(&accounts.token_vault)?;
        let quote_balance_after = token_balance(&accounts.quote_vault)?;
        let new_market_state = accounts.new_market_state.key();
        let curve = &mut ctx.accounts.curve;
        // Quote the target left behind goes to the fee recipient rather than staying
        // stranded in a closed curve.
        curve.creator_fees += quote_returned;
        curve.leftover_owed = leftover_owed;
        curve.token_reserve = token_balance_after;
        curve.quote_reserve = quote_balance_after;
        curve.status = Status::Graduated;
        curve.graduated_at = clock.unix_timestamp;
        curve.graduated_program = target;
        curve.graduated_market_state = new_market_state;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Parameters and state
// ---------------------------------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub enum Launch {
    /// Create a Token-2022 mint (the `token_mint` account, a fresh keypair) with built-in
    /// metadata. `metadata_authority` may later update the metadata; `None` freezes it.
    NewMint {
        decimals: u8,
        name: String,
        symbol: String,
        uri: String,
        metadata_authority: Option<Pubkey>,
    },
    /// Launch an existing mint, funded with `fund_curve`. Tokens left over at graduation
    /// go to `leftover_recipient`.
    ExistingMint { leftover_recipient: Pubkey },
}

/// What happens to LP tokens or positions the graduation target issues.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LpPolicy {
    /// Held by `["lp", curve]`, which nothing can move: locked for good.
    Locked,
    /// Sent to the incinerator.
    Burn,
    /// Sent to this wallet, which can withdraw the liquidity.
    Creator(Pubkey),
}

/// `create_market` setup data.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct InitParams {
    pub launch: Launch,
    /// Tokens in the launch, including any creator allocation.
    pub total_supply: u64,
    /// Share of `total_supply` sold on the curve.
    pub sold_bps: u16,
    /// Share of `total_supply` seeded into the graduated pool. It must be smaller than
    /// the sold share.
    pub pool_bps: u16,
    /// Share of `total_supply` handed to the graduation target on top of the pool's
    /// tokens, for AMMs that keep a separate token reserve. It doesn't enter the curve's
    /// prices. Whatever is left after the three shares is the creator allocation.
    pub reserve_bps: u16,
    /// Quote to raise (after fees) before the curve completes. With the split, it sets
    /// the starting price and how steep the curve is.
    pub graduation_threshold: u64,
    /// The creator's fee on every curve trade, in the quote token.
    pub curve_fee_bps: u16,
    pub fee_recipient: Pubkey,
    /// Owner of the creator allocation (the supply neither sold nor seeded).
    pub allocation_owner: Pubkey,
    /// 0 makes the allocation claimable at once; otherwise it unlocks linearly over this
    /// long from graduation.
    pub vest_seconds: i64,
    /// Optional anti-sniper fee: starts at `launch_fee_bps` and falls to 0 over
    /// `launch_fee_slots`. Proceeds go into the graduated pool.
    pub launch_fee_bps: u16,
    pub launch_fee_slots: u64,
    /// Slots after opening before the first trade.
    pub trading_delay_slots: u64,
    /// Registered strategy to graduate into, and its `pipeline_seed_liquidity` data.
    pub target_program: Pubkey,
    pub target_seed_data: Vec<u8>,
    /// Used if graduation into the target hasn't happened 3 days after completion.
    /// `None` means the playground constant-product strategy.
    pub fallback_program: Option<Pubkey>,
    pub fallback_seed_data: Vec<u8>,
    pub lp_policy: LpPolicy,
    /// Paid in the quote token to whoever runs `graduate`. At most 1% of
    /// `graduation_threshold`.
    pub graduation_reward: u64,
    /// How far the graduated pool's mid price may be from the price the curve offered
    /// it, in basis points. 10,000 (the most) turns the check off.
    pub price_tolerance_bps: u16,
}

impl InitParams {
    fn validate(&self) -> Result<()> {
        require!(
            self.curve_fee_bps <= MAX_CURVE_FEE_BPS,
            CurveError::BadInitData
        );
        require!(
            self.launch_fee_bps <= MAX_LAUNCH_FEE_BPS,
            CurveError::BadInitData
        );
        require!(self.vest_seconds >= 0, CurveError::BadInitData);
        require!(
            self.price_tolerance_bps <= MAX_PRICE_TOLERANCE_BPS,
            CurveError::BadInitData
        );
        // The default fallback is the playground pool, whose seed data is a 2-byte fee
        // below 100%. Checked now so a curve can't be left with an unusable fallback.
        if self.fallback_program.unwrap_or(DEFAULT_FALLBACK_PROGRAM) == DEFAULT_FALLBACK_PROGRAM {
            let fee = <[u8; 2]>::try_from(self.fallback_seed_data.as_slice())
                .map(u16::from_le_bytes)
                .map_err(|_| CurveError::BadInitData)?;
            require!((fee as u64) < BPS, CurveError::BadInitData);
        }
        require!(
            self.target_seed_data.len() <= MAX_SEED_DATA
                && self.fallback_seed_data.len() <= MAX_SEED_DATA,
            CurveError::BadInitData
        );
        require!(
            self.graduation_reward as u128 * BPS as u128
                <= self.graduation_threshold as u128 * MAX_GRADUATION_REWARD_BPS as u128,
            CurveError::BadInitData
        );
        if let Launch::NewMint {
            name, symbol, uri, ..
        } = &self.launch
        {
            require!(
                name.len() <= MAX_NAME && symbol.len() <= MAX_SYMBOL && uri.len() <= MAX_URI,
                CurveError::BadInitData
            );
        }
        Ok(())
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum Status {
    /// Existing-mint launch waiting for `fund_curve`.
    AwaitingFunds,
    Trading,
    /// The threshold is reached; waiting for `graduate`.
    Complete,
    Graduated,
}

#[account]
#[derive(InitSpace)]
pub struct Curve {
    pub status: Status,
    /// Whether setup created the mint (and so the curve's seeds omit `params_hash`).
    pub new_mint: bool,
    pub params_hash: [u8; 32],
    pub token_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub token_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub token_program: Pubkey,
    pub quote_program: Pubkey,
    /// The Pipeline market PDA allowed to call `pipeline_swap`.
    pub market_signer: Pubkey,
    pub virtual_quote: u64,
    pub virtual_tokens: u64,
    pub curve_tokens: u64,
    pub graduation_tokens: u64,
    /// Tokens handed to the target on top of the pool's, for its separate reserve. Not
    /// offered to the fallback target; then they are leftovers.
    pub reserve_tokens: u64,
    pub threshold: u64,
    pub sold: u64,
    pub raised: u64,
    pub total_supply: u64,
    /// Quote-vault buckets beyond `raised`.
    pub creator_fees: u64,
    pub launch_fees: u64,
    /// Vault balances after the last operation, for crediting swap input.
    pub token_reserve: u64,
    pub quote_reserve: u64,
    pub curve_fee_bps: u16,
    pub fee_recipient: Pubkey,
    pub allocation_owner: Pubkey,
    pub allocation_total: u64,
    pub allocation_claimed: u64,
    pub vest_seconds: i64,
    pub launch_fee_bps: u16,
    pub launch_fee_slots: u64,
    pub trading_delay_slots: u64,
    pub start_slot: u64,
    pub trading_opens_slot: u64,
    pub target_program: Pubkey,
    #[max_len(MAX_SEED_DATA)]
    pub target_seed_data: Vec<u8>,
    pub fallback_program: Pubkey,
    #[max_len(MAX_SEED_DATA)]
    pub fallback_seed_data: Vec<u8>,
    pub lp_recipient: Pubkey,
    pub leftover_recipient: Pubkey,
    /// Existing-mint launches: tokens left over at graduation, claimable by the
    /// leftover recipient.
    pub leftover_owed: u64,
    pub graduation_reward: u64,
    pub price_tolerance_bps: u16,
    pub completed_at: i64,
    pub graduated_at: i64,
    pub graduated_program: Pubkey,
    pub graduated_market_state: Pubkey,
    pub bump: u8,
}

impl Curve {
    fn math(&self) -> math::Curve {
        math::Curve {
            virtual_quote: self.virtual_quote,
            virtual_tokens: self.virtual_tokens,
            curve_tokens: self.curve_tokens,
            threshold: self.threshold,
            sold: self.sold,
            raised: self.raised,
        }
    }

    fn signer_seeds<'a>(&'a self, bump: &'a [u8]) -> Vec<&'a [u8]> {
        curve_signer_seeds(
            &self.token_mint,
            &self.quote_mint,
            self.new_mint,
            &self.params_hash,
            bump,
        )
    }

    /// Input and output vaults for a trade.
    fn vaults(&self, trade: &psi::TradeRequest) -> Result<(Pubkey, Pubkey)> {
        match (trade.input_index, trade.output_index) {
            (QUOTE_INDEX, TOKEN_INDEX) => Ok((self.quote_vault, self.token_vault)),
            (TOKEN_INDEX, QUOTE_INDEX) => Ok((self.token_vault, self.quote_vault)),
            _ => err!(CurveError::WrongAccount),
        }
    }

    fn launch_fee_now(&self, slot: u64) -> u16 {
        // Decays from when trading opens, so a trading delay doesn't use it up.
        math::launch_fee_bps(
            self.launch_fee_bps,
            self.trading_opens_slot,
            self.launch_fee_slots,
            slot,
        )
    }

    /// The single pricing function behind both `pipeline_quote` and `pipeline_swap`.
    fn fill(&self, trade: &psi::TradeRequest, amount: u64, slot: u64) -> Result<math::Fill> {
        self.vaults(trade)?;
        require!(self.status == Status::Trading, CurveError::WrongStatus);
        require!(slot >= self.trading_opens_slot, CurveError::NotOpenYet);
        let fee_bps = self.curve_fee_bps + self.launch_fee_now(slot);
        let curve = self.math();
        let fill = match (trade.input_index == QUOTE_INDEX, trade.mode) {
            (true, TradeMode::ExactIn) => curve.buy_exact_in(amount, fee_bps),
            (true, TradeMode::ExactOut) => curve.buy_exact_out(amount, fee_bps),
            (false, TradeMode::ExactIn) => curve.sell_exact_in(amount, fee_bps),
            (false, TradeMode::ExactOut) => curve.sell_exact_out(amount, fee_bps),
        };
        fill.ok_or(error!(CurveError::CannotFill))
    }

    fn claimable_allocation(&self, now: i64) -> u64 {
        // An existing-mint curve holds the allocation only once it's funded.
        let unlocked = if self.status == Status::AwaitingFunds {
            0
        } else if self.vest_seconds == 0 {
            self.allocation_total
        } else if self.status != Status::Graduated {
            0
        } else {
            let elapsed = (now - self.graduated_at).clamp(0, self.vest_seconds);
            (self.allocation_total as u128 * elapsed as u128 / self.vest_seconds as u128) as u64
        };
        unlocked.saturating_sub(self.allocation_claimed)
    }
}

/// The launch-fee part of a trade's fee, in proportion to the two fee rates.
fn launch_fee_share(fee: u64, curve_bps: u16, launch_bps: u16) -> u64 {
    let total = curve_bps as u128 + launch_bps as u128;
    if total == 0 {
        return 0;
    }
    (fee as u128 * launch_bps as u128 / total) as u64
}

fn curve_signer_seeds<'a>(
    mint: &'a Pubkey,
    quote_mint: &'a Pubkey,
    new_mint: bool,
    params_hash: &'a [u8; 32],
    bump: &'a [u8],
) -> Vec<&'a [u8]> {
    if new_mint {
        vec![b"curve", mint.as_ref(), bump]
    } else {
        vec![
            b"curve",
            mint.as_ref(),
            quote_mint.as_ref(),
            params_hash.as_ref(),
            bump,
        ]
    }
}

/// The curve's address: `["curve", mint]` for new mints (only the mint keypair's holder
/// can create it), otherwise `["curve", mint, quote_mint, sha256(init data)]`, so
/// nobody can occupy an existing-mint launch's address with other parameters or another
/// quote token.
pub fn curve_address(
    mint: &Pubkey,
    quote_mint: &Pubkey,
    new_mint: bool,
    params_hash: &[u8; 32],
) -> (Pubkey, u8) {
    if new_mint {
        Pubkey::find_program_address(&[b"curve", mint.as_ref()], &crate::ID)
    } else {
        Pubkey::find_program_address(
            &[b"curve", mint.as_ref(), quote_mint.as_ref(), params_hash],
            &crate::ID,
        )
    }
}

/// The PDA that holds LP for curves with [`LpPolicy::Locked`]. This program has no
/// instruction that signs for it.
pub fn lp_lock_address(curve: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"lp", curve.as_ref()], &crate::ID).0
}

/// The Pipeline market PDA for a curve of this program.
fn pipeline_market_signer(curve: &Pubkey) -> Pubkey {
    pipeline_market_signer_for(&crate::ID, curve)
}

/// The Pipeline market PDA for a market state of `program`.
fn pipeline_market_signer_for(program: &Pubkey, market_state: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            pipeline_seeds::MARKET,
            program.as_ref(),
            market_state.as_ref(),
        ],
        &Pubkey::new_from_array(psi::PIPELINE_PROGRAM_ID),
    )
    .0
}

/// The market a graduation has just created, for quoting it.
struct NewMarket<'a, 'info> {
    target: &'a AccountInfo<'info>,
    market_signer: Pubkey,
    market_state: Pubkey,
    token_mint: Pubkey,
    quote_mint: Pubkey,
    token_program: Pubkey,
    quote_program: Pubkey,
}

impl<'info> NewMarket<'_, 'info> {
    /// The target's `pipeline_quote` for an exact-in trade of `amount`: a buy (quote in,
    /// token out) or a sell. The new market's mints are in the order they were seeded:
    /// the token, then the quote token.
    ///
    /// `accounts` are the fixed trade accounts and then the target's own. The fixed ones
    /// that identify the trade must be this market's; the trader accounts are
    /// placeholders and the input vault is the target's to check. All are passed
    /// read-only, as quotes don't write.
    fn quote(
        &self,
        accounts: &[AccountInfo<'info>],
        buying: bool,
        amount: u64,
    ) -> Result<psi::QuoteResult> {
        use psi::trade_accounts as ix;
        require!(accounts.len() >= ix::LEN, CurveError::WrongAccount);
        let (input_index, output_index) = if buying {
            (QUOTE_INDEX, TOKEN_INDEX)
        } else {
            (TOKEN_INDEX, QUOTE_INDEX)
        };
        let (input_mint, output_mint, input_program, output_program) = if buying {
            (
                self.quote_mint,
                self.token_mint,
                self.quote_program,
                self.token_program,
            )
        } else {
            (
                self.token_mint,
                self.quote_mint,
                self.token_program,
                self.quote_program,
            )
        };
        for (index, expected) in [
            (ix::MARKET_SIGNER, self.market_signer),
            (ix::MARKET_STATE, self.market_state),
            (ix::INPUT_MINT, input_mint),
            (ix::OUTPUT_MINT, output_mint),
            (ix::INPUT_TOKEN_PROGRAM, input_program),
            (ix::OUTPUT_TOKEN_PROGRAM, output_program),
        ] {
            require_keys_eq!(accounts[index].key(), expected, CurveError::WrongAccount);
        }
        let request = psi::TradeRequest {
            mode: TradeMode::ExactIn,
            input_index,
            output_index,
            amount,
            trader: None,
            extra: vec![],
        };
        let metas = accounts
            .iter()
            .map(|account| AccountMeta::new_readonly(account.key(), false))
            .collect();
        let mut infos = accounts.to_vec();
        infos.push(self.target.clone());
        // Clear the target's seed result, so a quote that sets no return data can't be
        // read as one.
        set_return_data(&[]);
        invoke(
            &Instruction {
                program_id: self.target.key(),
                accounts: metas,
                data: psi::instruction_data(psi::discriminator::QUOTE, &request),
            },
            &infos,
        )?;
        let (program, data) = get_return_data().ok_or(CurveError::BadQuote)?;
        require_keys_eq!(program, self.target.key(), CurveError::BadQuote);
        psi::QuoteResult::try_from_slice(&data).map_err(|_| error!(CurveError::BadQuote))
    }
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

/// Creates a PDA account owned by `owner`, even if someone sent it lamports first.
fn create_pda_account<'info>(
    payer: &AccountInfo<'info>,
    account: &AccountInfo<'info>,
    space: usize,
    owner: &Pubkey,
    seeds: &[&[u8]],
) -> Result<()> {
    let rent = Rent::get()?.minimum_balance(space);
    let infos = [payer.clone(), account.clone()];
    if account.lamports() == 0 {
        invoke_signed(
            &system_instruction::create_account(payer.key, account.key, rent, space as u64, owner),
            &infos,
            &[seeds],
        )?;
        return Ok(());
    }
    let top_up = rent.saturating_sub(account.lamports());
    if top_up > 0 {
        invoke_signed(
            &system_instruction::transfer(payer.key, account.key, top_up),
            &infos,
            &[],
        )?;
    }
    invoke_signed(
        &system_instruction::allocate(account.key, space as u64),
        &[account.clone()],
        &[seeds],
    )?;
    invoke_signed(
        &system_instruction::assign(account.key, owner),
        &[account.clone()],
        &[seeds],
    )?;
    Ok(())
}

/// Creates the launch's Token-2022 mint with its metadata stored in the mint itself.
/// The curve is the mint authority until setup mints the supply and removes it. There
/// is never a freeze authority.
fn create_new_mint<'info>(
    accounts: &InitMarket<'info>,
    curve_seeds: &[&[u8]],
    decimals: u8,
    name: &str,
    symbol: &str,
    uri: &str,
    metadata_authority: Option<Pubkey>,
) -> Result<()> {
    let mint = accounts.token_mint.to_account_info();
    let curve = accounts.curve.to_account_info();
    let token_program = accounts.token_program.key();
    require!(mint.is_signer, CurveError::MintNotFresh);
    let base_space = ExtensionType::try_calculate_account_len::<spl_token_2022::state::Mint>(&[
        ExtensionType::MetadataPointer,
    ])?;
    // TLV header, update authority, mint, three strings and an empty additional list.
    let metadata_space = 4 + 32 + 32 + 4 + name.len() + 4 + symbol.len() + 4 + uri.len() + 4;
    let lamports = Rent::get()?.minimum_balance(base_space + metadata_space);
    invoke_signed(
        &system_instruction::create_account(
            accounts.payer.key,
            mint.key,
            lamports,
            base_space as u64,
            &token_program,
        ),
        &[accounts.payer.to_account_info(), mint.clone()],
        &[],
    )?;
    metadata_pointer_initialize(
        CpiContext::new(
            token_program,
            MetadataPointerInitialize {
                token_program_id: accounts.token_program.to_account_info(),
                mint: mint.clone(),
            },
        ),
        None,
        Some(mint.key()),
    )?;
    token_interface::initialize_mint2(
        CpiContext::new(
            token_program,
            token_interface::InitializeMint2 { mint: mint.clone() },
        ),
        decimals,
        &curve.key(),
        None,
    )?;
    token_metadata_initialize(
        CpiContext::new_with_signer(
            token_program,
            TokenMetadataInitialize {
                program_id: accounts.token_program.to_account_info(),
                metadata: mint.clone(),
                update_authority: curve.clone(),
                mint_authority: curve.clone(),
                mint: mint.clone(),
            },
            &[curve_seeds],
        ),
        name.to_string(),
        symbol.to_string(),
        uri.to_string(),
    )?;
    let new_authority =
        OptionalNonZeroPubkey::try_from(metadata_authority).map_err(|_| CurveError::BadInitData)?;
    token_metadata_update_authority(
        CpiContext::new_with_signer(
            token_program,
            TokenMetadataUpdateAuthority {
                program_id: accounts.token_program.to_account_info(),
                metadata: mint.clone(),
                current_authority: curve.clone(),
                new_authority: curve.clone(),
            },
            &[curve_seeds],
        ),
        new_authority,
    )?;
    Ok(())
}

fn borsh_encode<T: AnchorSerialize>(value: &T) -> Result<Vec<u8>> {
    let mut encoded = Vec::new();
    value.serialize(&mut encoded)?;
    Ok(encoded)
}

fn token_balance(account: &AccountInfo) -> Result<u64> {
    let data = account.try_borrow_data()?;
    require!(data.len() >= 72, CurveError::WrongAccount);
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
}

fn mint_decimals(mint: &AccountInfo) -> Result<u8> {
    let data = mint.try_borrow_data()?;
    require!(data.len() >= 82, CurveError::WrongAccount);
    Ok(data[44])
}

/// `transfer_checked` that forwards transfer-hook accounts to the token program.
#[allow(clippy::too_many_arguments)]
fn transfer_checked<'info>(
    token_program: &AccountInfo<'info>,
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    hook_accounts: &[AccountInfo<'info>],
    amount: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let decimals = mint_decimals(mint)?;
    let mut data = vec![12u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    let mut metas = vec![
        AccountMeta::new(from.key(), false),
        AccountMeta::new_readonly(mint.key(), false),
        AccountMeta::new(to.key(), false),
        AccountMeta::new_readonly(authority.key(), true),
    ];
    let mut infos = vec![from.clone(), mint.clone(), to.clone(), authority.clone()];
    for account in hook_accounts {
        metas.push(if account.is_writable {
            AccountMeta::new(account.key(), false)
        } else {
            AccountMeta::new_readonly(account.key(), false)
        });
        infos.push(account.clone());
    }
    infos.push(token_program.clone());
    invoke_signed(
        &Instruction {
            program_id: token_program.key(),
            accounts: metas,
            data,
        },
        &infos,
        signer_seeds,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------------------

/// Accounts passed by the router to `pipeline_init_market`: the four fixed accounts,
/// then this strategy's own.
#[derive(Accounts)]
pub struct InitMarket<'info> {
    pub market_signer: Signer<'info>,
    /// CHECK: the curve PDA, created and checked in the handler.
    #[account(mut)]
    pub curve: UncheckedAccount<'info>,
    /// The router's setup payer, funded by the market creator.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: a fresh keypair (new mint) or an existing mint; checked in the handler.
    #[account(mut)]
    pub token_mint: UncheckedAccount<'info>,
    pub quote_mint: InterfaceAccount<'info, Mint>,
    /// CHECK: the curve's associated token account; the associated token program checks it.
    #[account(mut)]
    pub token_vault: UncheckedAccount<'info>,
    /// CHECK: the curve's associated token account; the associated token program checks it.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct MarketInfoAccounts<'info> {
    pub curve: Account<'info, Curve>,
}

/// The fixed trade accounts, read-only, for quotes and account resolution. Only the
/// curve is read; the others may be placeholders.
#[derive(Accounts)]
pub struct ReadTrade<'info> {
    /// CHECK: unused in reads.
    pub market_signer: UncheckedAccount<'info>,
    pub curve: Account<'info, Curve>,
    /// CHECK: unused in reads.
    pub trader: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub trader_input: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub trader_output: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub input_vault: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub input_mint: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub output_mint: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub input_token_program: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub output_token_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct SwapAccounts<'info> {
    pub market_signer: Signer<'info>,
    #[account(mut)]
    pub curve: Account<'info, Curve>,
    /// CHECK: identity only; never a signer here.
    pub trader: UncheckedAccount<'info>,
    /// CHECK: receives refunds; the token program checks it.
    #[account(mut)]
    pub trader_input: UncheckedAccount<'info>,
    /// CHECK: receives output; the token program checks it.
    #[account(mut)]
    pub trader_output: UncheckedAccount<'info>,
    /// CHECK: compared with the curve's vault for the input mint.
    #[account(mut)]
    pub input_vault: UncheckedAccount<'info>,
    /// CHECK: one of the curve's mints; the token program checks it in transfers.
    pub input_mint: UncheckedAccount<'info>,
    /// CHECK: one of the curve's mints; the token program checks it in transfers.
    pub output_mint: UncheckedAccount<'info>,
    pub input_token_program: Interface<'info, TokenInterface>,
    pub output_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct FundCurve<'info> {
    pub funder: Signer<'info>,
    #[account(mut, has_one = token_mint, has_one = token_vault, has_one = token_program)]
    pub curve: Account<'info, Curve>,
    /// CHECK: matched against the curve.
    pub token_mint: UncheckedAccount<'info>,
    /// CHECK: the token program checks the source.
    #[account(mut)]
    pub funder_token: UncheckedAccount<'info>,
    /// CHECK: matched against the curve.
    #[account(mut)]
    pub token_vault: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ClaimCreatorFees<'info> {
    pub fee_recipient: Signer<'info>,
    #[account(
        mut,
        has_one = fee_recipient,
        has_one = quote_mint,
        has_one = quote_vault,
        constraint = curve.quote_program == quote_token_program.key() @ CurveError::WrongAccount,
    )]
    pub curve: Account<'info, Curve>,
    /// CHECK: matched against the curve.
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: matched against the curve.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: any token account of the quote mint; the token program checks it.
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,
    pub quote_token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct SetFeeRecipient<'info> {
    pub fee_recipient: Signer<'info>,
    #[account(mut, has_one = fee_recipient)]
    pub curve: Account<'info, Curve>,
}

#[derive(Accounts)]
pub struct ClaimAllocation<'info> {
    pub allocation_owner: Signer<'info>,
    #[account(
        mut,
        has_one = allocation_owner,
        has_one = token_mint,
        has_one = token_vault,
        has_one = token_program,
    )]
    pub curve: Account<'info, Curve>,
    /// CHECK: matched against the curve.
    pub token_mint: UncheckedAccount<'info>,
    /// CHECK: matched against the curve.
    #[account(mut)]
    pub token_vault: UncheckedAccount<'info>,
    /// CHECK: any token account of the token mint; the token program checks it.
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct ClaimLeftover<'info> {
    pub leftover_recipient: Signer<'info>,
    #[account(
        mut,
        has_one = leftover_recipient,
        has_one = token_mint,
        has_one = token_vault,
        has_one = token_program,
    )]
    pub curve: Account<'info, Curve>,
    /// CHECK: matched against the curve.
    pub token_mint: UncheckedAccount<'info>,
    /// CHECK: matched against the curve.
    #[account(mut)]
    pub token_vault: UncheckedAccount<'info>,
    /// CHECK: any token account of the token mint; the token program checks it.
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct Graduate<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,
    #[account(
        mut,
        has_one = token_mint,
        has_one = quote_mint,
        has_one = token_vault,
        has_one = quote_vault,
        has_one = token_program,
        constraint = curve.quote_program == quote_token_program.key() @ CurveError::WrongAccount,
    )]
    pub curve: Account<'info, Curve>,
    /// CHECK: matched against the curve; writable for burning leftovers.
    #[account(mut)]
    pub token_mint: UncheckedAccount<'info>,
    /// CHECK: matched against the curve.
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: matched against the curve.
    #[account(mut)]
    pub token_vault: UncheckedAccount<'info>,
    /// CHECK: matched against the curve.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    /// CHECK: PDA that owns the escrows and signs for the target.
    #[account(seeds = [b"seed", curve.key().as_ref()], bump)]
    pub seed_authority: UncheckedAccount<'info>,
    /// CHECK: the seed authority's associated token account; created idempotently.
    #[account(mut)]
    pub token_escrow: UncheckedAccount<'info>,
    /// CHECK: the seed authority's associated token account; created idempotently.
    #[account(mut)]
    pub quote_escrow: UncheckedAccount<'info>,
    /// CHECK: PDA lent the caller's bounded SOL to pay the target's rent.
    #[account(mut, seeds = [b"rent", curve.key().as_ref()], bump)]
    pub rent_payer: UncheckedAccount<'info>,
    /// CHECK: matched against the curve's LP policy.
    pub lp_recipient: UncheckedAccount<'info>,
    /// CHECK: any token account of the quote mint; the token program checks it.
    #[account(mut)]
    pub reward_account: UncheckedAccount<'info>,
    /// CHECK: the curve's target or fallback; checked in the handler.
    pub target_program: UncheckedAccount<'info>,
    /// CHECK: the router's registration of the target; checked in the handler.
    pub target_registration: UncheckedAccount<'info>,
    /// CHECK: created by the target; its owner is checked afterwards.
    #[account(mut)]
    pub new_market_state: UncheckedAccount<'info>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[error_code]
pub enum CurveError {
    #[msg("Init data is not valid InitParams")]
    BadInitData,
    #[msg("Caller is not this curve's Pipeline market")]
    NotPipeline,
    #[msg("Unexpected account")]
    WrongAccount,
    #[msg("A new mint must be a fresh keypair that signs")]
    MintNotFresh,
    #[msg("Not allowed in the curve's current status")]
    WrongStatus,
    #[msg("Trading has not opened yet")]
    NotOpenYet,
    #[msg("The curve cannot fill that trade")]
    CannotFill,
    #[msg("Not enough input for the requested output")]
    InsufficientInput,
    #[msg("The fallback target is only allowed 3 days after completion")]
    FallbackTooEarly,
    #[msg("The target strategy is not registered with Pipeline")]
    TargetNotRegistered,
    #[msg("Nothing to graduate")]
    NothingToGraduate,
    #[msg("The target did not create and fund the new market")]
    TargetFailed,
    #[msg("The split can't open the graduated pool at the curve's final price")]
    BadSplit,
    #[msg("The target's quote of the new pool could not be read")]
    BadQuote,
    #[msg("The new pool's price is outside the curve's price tolerance")]
    PoolOffPrice,
}
