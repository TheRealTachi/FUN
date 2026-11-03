//! fun transfer hook for Meteora DBC transfer-hook pools. Two rules, fixed at
//! launch:
//!
//!   * Max hold: no wallet may end a transfer holding more than the coin's
//!     cap, which its launcher picks between 0.5% and 4% of the supply. The curve's vault and the launcher's wallet (which burns
//!     everything it buys back) are exempt, and so is anything the launcher's
//!     wallet sends: that is only ever the dev buy, going into its 24-hour
//!     Jupiter Lock and later out to the dev.
//!   * No transfers (unless the launcher opens them): before bonding, tokens
//!     only move to and from the curve (buys and sells). Wallet-to-wallet sends
//!     are refused, so nobody can gather several wallets' supply into one. The
//!     launcher's wallet is the exception: it moves the dev buy into its lock
//!     and out to the dev. With transfers open, receiving wallets still can't
//!     pass the max hold or be blocklisted. A quick-bond coin can't open them:
//!     a side pool would let holders sell around the lock.
//!   * Quick bond (optional): no selling until the coin bonds. As a safety
//!     valve the launcher sets a time (at most 24 hours) after which selling
//!     opens anyway, so buyers are never stuck with a coin that never fills.
//!   * Blocklist (optional): wallets the launcher listed before launch can't
//!     receive the coin, so they can't buy it. The list is written before the
//!     pool exists and sealed by `initialize`; after that nobody can change it.
//!     Listed wallets can still send or sell anything they hold.
//!
//!   * App rules (optional, coins launched with `initialize_with_apps`): the
//!     launcher picks trading apps (pump.fun app, FOMO, Axiom, GMGN, Terminal)
//!     and either blocks buys made through them or allows buys only through
//!     them. Each app leaves a fingerprint in the transaction's top-level
//!     instructions, read from the instructions sysvar. Coins launched with
//!     `initialize` never pass those accounts, so the check never runs for them.
//!
//! Snipers are handled by the pool, not here: each coin's fee is 99% for its
//! first few blocks, and that fee buys the coin back and burns it. The dev
//! buy is locked in a Jupiter Lock escrow, not here, so that lock outlives
//! bonding.
//!
//! When the curve completes, DBC revokes the hook in that swap and this
//! program is never called again: both rules end at bonding. The blocklist's
//! rent can then be reclaimed with `close_blocklist`.
//!
//! There is no admin and nothing to configure. The launch record is written
//! once, by `initialize`, in the same transaction that creates the pool, and
//! Token-2022 fails every transfer until the extra-account-meta list exists,
//! so no transfer can happen before the rules are in place.

use anchor_lang::{
    prelude::*,
    solana_program::{
        program::invoke_signed,
        system_instruction,
        sysvar::instructions::{self as ix_sysvar, load_instruction_at_checked},
    },
};
use anchor_spl::{
    token_2022::spl_token_2022::{
        extension::{transfer_hook::TransferHook, BaseStateWithExtensions, StateWithExtensions},
        state::{Account as TokenAccountState, Mint as MintState},
    },
    token_interface::Mint,
};
use spl_discriminator::SplDiscriminate;
use spl_tlv_account_resolution::{account::ExtraAccountMeta, seeds::Seed, state::ExtraAccountMetaList};
use spl_transfer_hook_interface::instruction::ExecuteInstruction;

declare_id!("FUNrSeDa9WjjJnAtZJpy7195jdee1VnWtEDU5nMW9Q7b");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "fun",
    project_url: "https://funpad.meme",
    contacts: "link:https://x.com/launchviafun",
    policy: "https://x.com/launchviafun",
    preferred_languages: "en",
    source_code: "https://github.com/TheRealTachi/FUN"
}

/// Meteora DBC pool authority PDA, owner of every DBC base vault.
pub const DBC_POOL_AUTHORITY: Pubkey = pubkey!("FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM");

pub const LAUNCH_SEED: &[u8] = b"launch";
pub const BLOCKLIST_SEED: &[u8] = b"blocklist";
/// Most wallets one coin may block.
pub const MAX_BLOCKED: usize = 2000;

/// Blocklist account layout. Kept by hand rather than as an Anchor account so
/// it can grow a chunk at a time and be searched in place:
///   0..8   discriminator
///   8..40  mint
///   40..72 authority (the coin's launch wallet)
///   72     sealed (1 once the coin launched; only sealed lists are enforced)
///   73     bump
///   74..78 count (u32 LE)
///   78..   count wallets, 32 bytes each, sorted ascending with no repeats
pub const BL_DISCRIMINATOR: [u8; 8] = *b"funblock";
pub const BL_HEADER: usize = 78;
/// The range a launcher may pick the max hold from, in basis points of the supply.
pub const MIN_HOLD_BPS: u16 = 50;
pub const MAX_HOLD_BPS: u16 = 400;
/// Bounds on a quick-bond coin's sell lock, in seconds (0 = selling always open).
pub const MIN_SELL_LOCK: u32 = 10;
pub const MAX_SELL_LOCK: u32 = 24 * 3600;
/// Passed as `sell_lock_secs` to launch with wallet-to-wallet sends allowed.
pub const OPEN_TRANSFERS_ARG: u32 = u32::MAX;
/// Stored in `sells_open_at` for such a coin. Real values are 0 or a unix time,
/// so coins launched before this option keep their rules and account layout.
pub const TRANSFERS_OPEN: i64 = -1;

pub const APPS_SEED: &[u8] = b"apps";
/// The apps a launcher can pick, one bit each.
pub const APP_PUMP: u8 = 1 << 0;
pub const APP_FOMO: u8 = 1 << 1;
pub const APP_AXIOM: u8 = 1 << 2;
pub const APP_GMGN: u8 = 1 << 3;
pub const APP_TERMINAL: u8 = 1 << 4;
pub const APP_ALL: u8 = APP_PUMP | APP_FOMO | APP_AXIOM | APP_GMGN | APP_TERMINAL;
/// Buys through the picked apps are refused; buys from anywhere else go through.
pub const APPS_BLOCK: u8 = 0;
/// Only buys through one of the picked apps go through.
pub const APPS_ONLY: u8 = 1;
/// Each app's fingerprint. The pump.fun app, Axiom, GMGN and Terminal route every
/// trade through their own program, called as a top-level instruction. FOMO's
/// relayer co-signs every FOMO trade, as a signer in one of its instructions.
pub const PUMP_APP_PROGRAM: Pubkey = pubkey!("6Vo3245eszAb5wuqEMw8mGdbfRUdKbHhDHP5LcaGuTAB");
pub const AXIOM_PROGRAM: Pubkey = pubkey!("FLASHX8DrLbgeR8FcfNV1F5krxYcYMUdBkrP1EPBtxB9");
pub const GMGN_PROGRAM: Pubkey = pubkey!("GMgnVFR8Jb39LoXsEVzb3DvBy3ywCmdmJquHUy1Lrkqb");
pub const TERMINAL_PROGRAM: Pubkey = pubkey!("term9YPb9mzAsABaqN71A4xdbxHmpBNZavpBiQKZzN3");
#[cfg(not(feature = "test-relayer"))]
pub const FOMO_RELAYER: Pubkey = pubkey!("AgmLJBMDCqWynYnQiPCuj9ewsNNsBJXyzoUhD9LJzN51");
/// A key the localnet tests can sign with, standing in for FOMO's relayer.
#[cfg(feature = "test-relayer")]
pub const FOMO_RELAYER: Pubkey = pubkey!("GeRG9MD1BrT6hApb2DTRtZy7YpzAf4nbyFVjFZxYJueY");

#[program]
pub mod unsniped_hook {
    use super::*;

    /// Creates the launch record and the extra-account-meta list for a mint.
    /// Send it in the same transaction as the DBC pool creation. The payer is
    /// recorded as the launcher (the coin's own wallet, which burns what it
    /// buys back).
    pub fn initialize(ctx: Context<Initialize>, max_hold_bps: u16, sell_lock_secs: u32) -> Result<()> {
        require!((MIN_HOLD_BPS..=MAX_HOLD_BPS).contains(&max_hold_bps), HookError::BadMaxHold);
        require!(
            sell_lock_secs == 0 || sell_lock_secs == OPEN_TRANSFERS_ARG || (MIN_SELL_LOCK..=MAX_SELL_LOCK).contains(&sell_lock_secs),
            HookError::BadSellLock
        );
        // Only mints that actually point their hook at this program.
        let supply;
        {
            let mint_info = ctx.accounts.mint.to_account_info();
            let data = mint_info.try_borrow_data()?;
            let mint = StateWithExtensions::<MintState>::unpack(&data)?;
            let hook = mint.get_extension::<TransferHook>()?;
            require!(
                Option::<Pubkey>::from(hook.program_id) == Some(crate::ID),
                HookError::WrongHookProgram
            );
            supply = mint.base.supply;
        }

        let mint_key = ctx.accounts.mint.key();

        // Extra account the token program must pass to `execute`:
        // index 5 = launch PDA, derived from the literal seed + mint (index 1).
        // Static seeds keep it compatible with the DBC SDK, which resolves
        // hook accounts with placeholder source/destination keys.
        // index 6 = blocklist PDA, the same way. It may not exist (no list).
        let metas = [
            ExtraAccountMeta::new_with_seeds(
                &[
                    Seed::Literal { bytes: LAUNCH_SEED.to_vec() },
                    Seed::AccountKey { index: 1 },
                ],
                false,
                false,
            )?,
            ExtraAccountMeta::new_with_seeds(
                &[
                    Seed::Literal { bytes: BLOCKLIST_SEED.to_vec() },
                    Seed::AccountKey { index: 1 },
                ],
                false,
                false,
            )?,
        ];
        let metas_len = ExtraAccountMetaList::size_of(metas.len())?;
        let lamports = Rent::get()?.minimum_balance(metas_len);
        invoke_signed(
            &system_instruction::create_account(
                ctx.accounts.payer.key,
                ctx.accounts.extra_account_meta_list.key,
                lamports,
                metas_len as u64,
                &crate::ID,
            ),
            &[
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.extra_account_meta_list.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
            &[&[b"extra-account-metas", mint_key.as_ref(), &[ctx.bumps.extra_account_meta_list]]],
        )?;
        ExtraAccountMetaList::init::<ExecuteInstruction>(
            &mut ctx.accounts.extra_account_meta_list.try_borrow_mut_data()?,
            &metas,
        )?;

        // Seal the launcher's blocklist, if they made one. A list made by
        // anyone else blocks the launch instead of being sealed.
        let blocked = {
            let bl = ctx.accounts.blocklist.to_account_info();
            if bl.data_is_empty() {
                0
            } else {
                require_keys_eq!(*bl.owner, crate::ID, HookError::BadBlocklist);
                let mut d = bl.try_borrow_mut_data()?;
                require!(d.len() >= BL_HEADER && d[..8] == BL_DISCRIMINATOR, HookError::BadBlocklist);
                require!(d[8..40] == mint_key.to_bytes(), HookError::BadBlocklist);
                require!(d[40..72] == ctx.accounts.payer.key().to_bytes(), HookError::NotListOwner);
                d[72] = 1;
                u32::from_le_bytes(d[74..78].try_into().unwrap())
            }
        };

        let clock = Clock::get()?;
        let launch = &mut ctx.accounts.launch;
        launch.mint = mint_key;
        launch.creator = ctx.accounts.payer.key();
        launch.launch_slot = clock.slot;
        launch.launched_at = clock.unix_timestamp;
        launch.max_hold = (supply as u128 * max_hold_bps as u128 / 10_000) as u64;
        launch.bump = ctx.bumps.launch;
        launch.sells_open_at = match sell_lock_secs {
            0 => 0,
            OPEN_TRANSFERS_ARG => TRANSFERS_OPEN,
            secs => clock.unix_timestamp + secs as i64,
        };

        emit!(Launched {
            mint: mint_key,
            creator: launch.creator,
            launch_slot: launch.launch_slot,
            max_hold: launch.max_hold,
            blocked,
        });
        Ok(())
    }

    /// `initialize`, plus app rules: which trading apps this coin's buys may
    /// (APPS_ONLY) or may not (APPS_BLOCK) come through, as APP_* bits. The
    /// rules are written to the coin's apps PDA, and the extra-account-meta
    /// list gains the instructions sysvar and that PDA, so every transfer
    /// passes them to `execute`. Kept separate from `initialize` so coins
    /// launched that way are untouched.
    pub fn initialize_with_apps(
        ctx: Context<InitializeWithApps>,
        max_hold_bps: u16,
        sell_lock_secs: u32,
        apps_mode: u8,
        apps: u8,
    ) -> Result<()> {
        require!((MIN_HOLD_BPS..=MAX_HOLD_BPS).contains(&max_hold_bps), HookError::BadMaxHold);
        require!(
            sell_lock_secs == 0 || sell_lock_secs == OPEN_TRANSFERS_ARG || (MIN_SELL_LOCK..=MAX_SELL_LOCK).contains(&sell_lock_secs),
            HookError::BadSellLock
        );
        require!(apps_mode == APPS_BLOCK || apps_mode == APPS_ONLY, HookError::BadAppRules);
        require!(apps != 0 && apps & !APP_ALL == 0, HookError::BadAppRules);
        let supply;
        {
            let mint_info = ctx.accounts.mint.to_account_info();
            let data = mint_info.try_borrow_data()?;
            let mint = StateWithExtensions::<MintState>::unpack(&data)?;
            let hook = mint.get_extension::<TransferHook>()?;
            require!(
                Option::<Pubkey>::from(hook.program_id) == Some(crate::ID),
                HookError::WrongHookProgram
            );
            supply = mint.base.supply;
        }

        let mint_key = ctx.accounts.mint.key();

        // index 5 = launch PDA, index 6 = blocklist PDA (as in `initialize`),
        // index 7 = instructions sysvar, index 8 = apps PDA.
        let metas = [
            ExtraAccountMeta::new_with_seeds(
                &[
                    Seed::Literal { bytes: LAUNCH_SEED.to_vec() },
                    Seed::AccountKey { index: 1 },
                ],
                false,
                false,
            )?,
            ExtraAccountMeta::new_with_seeds(
                &[
                    Seed::Literal { bytes: BLOCKLIST_SEED.to_vec() },
                    Seed::AccountKey { index: 1 },
                ],
                false,
                false,
            )?,
            ExtraAccountMeta::new_with_pubkey(&ix_sysvar::ID, false, false)?,
            ExtraAccountMeta::new_with_seeds(
                &[
                    Seed::Literal { bytes: APPS_SEED.to_vec() },
                    Seed::AccountKey { index: 1 },
                ],
                false,
                false,
            )?,
        ];
        let metas_len = ExtraAccountMetaList::size_of(metas.len())?;
        let lamports = Rent::get()?.minimum_balance(metas_len);
        invoke_signed(
            &system_instruction::create_account(
                ctx.accounts.payer.key,
                ctx.accounts.extra_account_meta_list.key,
                lamports,
                metas_len as u64,
                &crate::ID,
            ),
            &[
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.extra_account_meta_list.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
            &[&[b"extra-account-metas", mint_key.as_ref(), &[ctx.bumps.extra_account_meta_list]]],
        )?;
        ExtraAccountMetaList::init::<ExecuteInstruction>(
            &mut ctx.accounts.extra_account_meta_list.try_borrow_mut_data()?,
            &metas,
        )?;

        let blocked = {
            let bl = ctx.accounts.blocklist.to_account_info();
            if bl.data_is_empty() {
                0
            } else {
                require_keys_eq!(*bl.owner, crate::ID, HookError::BadBlocklist);
                let mut d = bl.try_borrow_mut_data()?;
                require!(d.len() >= BL_HEADER && d[..8] == BL_DISCRIMINATOR, HookError::BadBlocklist);
                require!(d[8..40] == mint_key.to_bytes(), HookError::BadBlocklist);
                require!(d[40..72] == ctx.accounts.payer.key().to_bytes(), HookError::NotListOwner);
                d[72] = 1;
                u32::from_le_bytes(d[74..78].try_into().unwrap())
            }
        };

        let rules = &mut ctx.accounts.app_rules;
        rules.mint = mint_key;
        rules.mode = apps_mode;
        rules.apps = apps;
        rules.bump = ctx.bumps.app_rules;

        let clock = Clock::get()?;
        let launch = &mut ctx.accounts.launch;
        launch.mint = mint_key;
        launch.creator = ctx.accounts.payer.key();
        launch.launch_slot = clock.slot;
        launch.launched_at = clock.unix_timestamp;
        launch.max_hold = (supply as u128 * max_hold_bps as u128 / 10_000) as u64;
        launch.bump = ctx.bumps.launch;
        launch.sells_open_at = match sell_lock_secs {
            0 => 0,
            OPEN_TRANSFERS_ARG => TRANSFERS_OPEN,
            secs => clock.unix_timestamp + secs as i64,
        };

        emit!(Launched {
            mint: mint_key,
            creator: launch.creator,
            launch_slot: launch.launch_slot,
            max_hold: launch.max_hold,
            blocked,
        });
        emit!(AppsSet { mint: mint_key, mode: apps_mode, apps });
        Ok(())
    }

    /// Starts an empty blocklist for a mint that hasn't launched yet. The
    /// signer becomes its authority and must be the wallet that launches it.
    pub fn open_blocklist(ctx: Context<OpenBlocklist>) -> Result<()> {
        let mint_key = ctx.accounts.mint.key();
        let lamports = Rent::get()?.minimum_balance(BL_HEADER);
        invoke_signed(
            &system_instruction::create_account(
                ctx.accounts.authority.key,
                ctx.accounts.blocklist.key,
                lamports,
                BL_HEADER as u64,
                &crate::ID,
            ),
            &[
                ctx.accounts.authority.to_account_info(),
                ctx.accounts.blocklist.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
            &[&[BLOCKLIST_SEED, mint_key.as_ref(), &[ctx.bumps.blocklist]]],
        )?;
        let mut d = ctx.accounts.blocklist.try_borrow_mut_data()?;
        d[..8].copy_from_slice(&BL_DISCRIMINATOR);
        d[8..40].copy_from_slice(&mint_key.to_bytes());
        d[40..72].copy_from_slice(&ctx.accounts.authority.key().to_bytes());
        d[72] = 0;
        d[73] = ctx.bumps.blocklist;
        d[74..78].copy_from_slice(&0u32.to_le_bytes());
        Ok(())
    }

    /// Appends wallets to an unsealed blocklist. Each call's wallets must be
    /// in ascending order and above every wallet already listed, so the list
    /// stays sorted and free of repeats (send the whole list sorted, in chunks).
    pub fn add_blocked(ctx: Context<EditBlocklist>, wallets: Vec<Pubkey>) -> Result<()> {
        let bl = ctx.accounts.blocklist.to_account_info();
        let count = {
            let d = bl.try_borrow_data()?;
            check_list(&d, &ctx.accounts.mint.key(), &ctx.accounts.authority.key())?;
            require!(d[72] == 0, HookError::Sealed);
            u32::from_le_bytes(d[74..78].try_into().unwrap()) as usize
        };
        let total = count + wallets.len();
        require!(total <= MAX_BLOCKED, HookError::TooManyBlocked);

        let new_len = BL_HEADER + total * 32;
        let need = Rent::get()?.minimum_balance(new_len).saturating_sub(bl.lamports());
        if need > 0 {
            anchor_lang::system_program::transfer(
                CpiContext::new(
                    ctx.accounts.system_program.to_account_info(),
                    anchor_lang::system_program::Transfer {
                        from: ctx.accounts.authority.to_account_info(),
                        to: bl.clone(),
                    },
                ),
                need,
            )?;
        }
        bl.realloc(new_len, false)?;

        let mut d = bl.try_borrow_mut_data()?;
        let mut last: Option<[u8; 32]> = if count > 0 {
            Some(d[BL_HEADER + (count - 1) * 32..BL_HEADER + count * 32].try_into().unwrap())
        } else {
            None
        };
        for (i, w) in wallets.iter().enumerate() {
            let k = w.to_bytes();
            if let Some(prev) = last {
                require!(k > prev, HookError::Unsorted);
            }
            let at = BL_HEADER + (count + i) * 32;
            d[at..at + 32].copy_from_slice(&k);
            last = Some(k);
        }
        d[74..78].copy_from_slice(&(total as u32).to_le_bytes());
        Ok(())
    }

    /// Gives a blocklist's rent back to its authority: any time before launch
    /// (a launch that never happened), or once the coin has bonded and DBC has
    /// switched the hook off, when the list no longer does anything.
    pub fn close_blocklist(ctx: Context<CloseBlocklist>) -> Result<()> {
        let bl = ctx.accounts.blocklist.to_account_info();
        let sealed = {
            let d = bl.try_borrow_data()?;
            check_list(&d, &ctx.accounts.mint.key(), &ctx.accounts.authority.key())?;
            d[72] == 1
        };
        if sealed {
            let mint_info = ctx.accounts.mint.to_account_info();
            let data = mint_info.try_borrow_data()?;
            let mint = StateWithExtensions::<MintState>::unpack(&data)?;
            let hook = mint.get_extension::<TransferHook>()?;
            require!(Option::<Pubkey>::from(hook.program_id).is_none(), HookError::StillActive);
        }
        let to = ctx.accounts.authority.to_account_info();
        **to.try_borrow_mut_lamports()? += bl.lamports();
        **bl.try_borrow_mut_lamports()? = 0;
        bl.assign(&anchor_lang::solana_program::system_program::ID);
        bl.realloc(0, false)?;
        Ok(())
    }

    /// Called by Token-2022 on every transfer while the hook is active.
    /// Read-only, so calling it directly achieves nothing.
    #[instruction(discriminator = ExecuteInstruction::SPL_DISCRIMINATOR_SLICE)]
    pub fn execute(ctx: Context<Execute>, _amount: u64) -> Result<()> {
        let launch = &ctx.accounts.launch;
        require_keys_eq!(launch.mint, ctx.accounts.mint.key(), HookError::BadLaunch);
        let (to, to_balance) = owner_and_amount(&ctx.accounts.destination_token)?;
        let (from, _) = owner_and_amount(&ctx.accounts.source_token)?;
        // A sell: into the curve's vault. On a quick-bond coin, refused until
        // its sell lock runs out (bonding ends the hook, and the lock, sooner).
        if to == DBC_POOL_AUTHORITY {
            if launch.sells_open_at > 0 && from != launch.creator && Clock::get()?.unix_timestamp < launch.sells_open_at {
                msg!("Sells locked: this quick-bond coin can't be sold until it bonds or {} passes", launch.sells_open_at);
                return err!(HookError::SellsLocked);
            }
            return Ok(());
        }
        // The launcher's wallet: buybacks it burns, and the dev buy coming back from its lock.
        if to == launch.creator {
            return Ok(());
        }
        if is_blocked(&ctx.accounts.blocklist, &launch.mint, &to)? {
            msg!("Blocked: {} can't buy this coin until it bonds", to);
            return err!(HookError::Blocked);
        }
        // The launcher's own wallet only ever sends the dev buy: into its
        // 24-hour lock, then out to the dev. Locked tokens can't be dumped, so
        // neither the cap nor the no-transfer rule applies to them.
        if from == launch.creator {
            return Ok(());
        }
        // Everything else must be a buy from the curve, unless this coin opened
        // wallet-to-wallet sends at launch.
        if from != DBC_POOL_AUTHORITY && launch.sells_open_at != TRANSFERS_OPEN {
            msg!("No transfers: {} can't send this coin to {} until it bonds", from, to);
            return err!(HookError::NoTransfers);
        }
        // App rules, on buys from the curve. Only coins launched with
        // `initialize_with_apps` list these two accounts, and Token-2022 then
        // always passes them; every other coin's transfers skip this.
        if from == DBC_POOL_AUTHORITY {
            if let [ixs, rules, ..] = ctx.remaining_accounts {
                check_apps(ixs, rules, &launch.mint, &to)?;
            }
        }
        // Token-2022 calls the hook after moving the tokens, so this is the
        // destination's balance including this transfer.
        if to_balance > launch.max_hold {
            msg!("Max hold: {} would hold {} of a {} cap", to, to_balance, launch.max_hold);
            return err!(HookError::MaxHold);
        }
        Ok(())
    }
}

/// The header checks every blocklist instruction shares.
fn check_list(d: &[u8], mint: &Pubkey, authority: &Pubkey) -> Result<()> {
    require!(d.len() >= BL_HEADER && d[..8] == BL_DISCRIMINATOR, HookError::BadBlocklist);
    require!(d[8..40] == mint.to_bytes(), HookError::BadBlocklist);
    require!(d[40..72] == authority.to_bytes(), HookError::NotListOwner);
    Ok(())
}

/// Whether `wallet` is on this coin's sealed blocklist. Binary search, in place.
fn is_blocked(info: &AccountInfo, mint: &Pubkey, wallet: &Pubkey) -> Result<bool> {
    if info.data_is_empty() || *info.owner != crate::ID {
        return Ok(false);
    }
    let d = info.try_borrow_data()?;
    if d.len() < BL_HEADER || d[..8] != BL_DISCRIMINATOR || d[8..40] != mint.to_bytes() || d[72] != 1 {
        return Ok(false);
    }
    let count = (u32::from_le_bytes(d[74..78].try_into().unwrap()) as usize).min((d.len() - BL_HEADER) / 32);
    let key = wallet.to_bytes();
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let at = BL_HEADER + mid * 32;
        match d[at..at + 32].cmp(&key[..]) {
            std::cmp::Ordering::Equal => return Ok(true),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    Ok(false)
}

/// Refuses a buy that breaks the coin's app rules.
fn check_apps(ixs: &AccountInfo, rules: &AccountInfo, mint: &Pubkey, buyer: &Pubkey) -> Result<()> {
    require_keys_eq!(*ixs.key, ix_sysvar::ID, HookError::BadAppRules);
    require_keys_eq!(*rules.owner, crate::ID, HookError::BadAppRules);
    let r = AppRules::try_deserialize(&mut &rules.try_borrow_data()?[..])?;
    require_keys_eq!(r.mint, *mint, HookError::BadAppRules);
    let used = apps_used(ixs)?;
    let allowed = if r.mode == APPS_ONLY { used & r.apps != 0 } else { used & r.apps == 0 };
    if !allowed {
        msg!("App rules: {} can't buy this coin through this app (mode {}, apps {:#07b}, used {:#07b})", buyer, r.mode, r.apps, used);
        return err!(HookError::AppNotAllowed);
    }
    Ok(())
}

/// The APP_* bits whose fingerprints are in this transaction's top-level instructions.
fn apps_used(ixs: &AccountInfo) -> Result<u8> {
    let count = {
        let d = ixs.try_borrow_data()?;
        require!(d.len() >= 2, HookError::BadAppRules);
        u16::from_le_bytes([d[0], d[1]]) as usize
    };
    let mut used = 0u8;
    for i in 0..count {
        let ix = load_instruction_at_checked(i, ixs)?;
        used |= match ix.program_id {
            PUMP_APP_PROGRAM => APP_PUMP,
            AXIOM_PROGRAM => APP_AXIOM,
            GMGN_PROGRAM => APP_GMGN,
            TERMINAL_PROGRAM => APP_TERMINAL,
            _ => 0,
        };
        if ix.accounts.iter().any(|a| a.pubkey == FOMO_RELAYER && a.is_signer) {
            used |= APP_FOMO;
        }
    }
    Ok(used)
}

fn owner_and_amount(info: &AccountInfo) -> Result<(Pubkey, u64)> {
    let data = info.try_borrow_data()?;
    let account = StateWithExtensions::<TokenAccountState>::unpack(&data)?;
    Ok((account.base.owner, account.base.amount))
}

#[account]
#[derive(InitSpace)]
pub struct Launch {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub launch_slot: u64,
    pub launched_at: i64,
    pub bump: u8,
    /// Most any wallet but the vault and the launcher may hold, in raw units.
    pub max_hold: u64,
    /// Quick bond: unix time selling opens if the coin hasn't bonded by then (0 = never locked).
    /// TRANSFERS_OPEN = never locked, and wallet-to-wallet sends are allowed.
    pub sells_open_at: i64,
}

/// A coin's app rules, written once by `initialize_with_apps`.
#[account]
#[derive(InitSpace)]
pub struct AppRules {
    pub mint: Pubkey,
    /// APPS_BLOCK or APPS_ONLY.
    pub mode: u8,
    /// APP_* bits.
    pub apps: u8,
    pub bump: u8,
}

#[event]
pub struct AppsSet {
    pub mint: Pubkey,
    pub mode: u8,
    pub apps: u8,
}

#[event]
pub struct Launched {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub launch_slot: u64,
    pub max_hold: u64,
    /// Wallets on the sealed blocklist (0 if none).
    pub blocked: u32,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: created here
    #[account(mut, seeds = [b"extra-account-metas", mint.key().as_ref()], bump)]
    pub extra_account_meta_list: UncheckedAccount<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Launch::INIT_SPACE,
        seeds = [LAUNCH_SEED, mint.key().as_ref()],
        bump
    )]
    pub launch: Account<'info, Launch>,
    /// CHECK: the launcher's blocklist PDA, sealed here if it exists
    #[account(mut, seeds = [BLOCKLIST_SEED, mint.key().as_ref()], bump)]
    pub blocklist: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct InitializeWithApps<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: created here
    #[account(mut, seeds = [b"extra-account-metas", mint.key().as_ref()], bump)]
    pub extra_account_meta_list: UncheckedAccount<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Launch::INIT_SPACE,
        seeds = [LAUNCH_SEED, mint.key().as_ref()],
        bump
    )]
    pub launch: Account<'info, Launch>,
    /// CHECK: the launcher's blocklist PDA, sealed here if it exists
    #[account(mut, seeds = [BLOCKLIST_SEED, mint.key().as_ref()], bump)]
    pub blocklist: UncheckedAccount<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + AppRules::INIT_SPACE,
        seeds = [APPS_SEED, mint.key().as_ref()],
        bump
    )]
    pub app_rules: Account<'info, AppRules>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct OpenBlocklist<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: a mint that doesn't exist yet; only its address is used
    pub mint: UncheckedAccount<'info>,
    /// CHECK: created here
    #[account(mut, seeds = [BLOCKLIST_SEED, mint.key().as_ref()], bump)]
    pub blocklist: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct EditBlocklist<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: only its address is used
    pub mint: UncheckedAccount<'info>,
    /// CHECK: checked in the handler
    #[account(mut, owner = crate::ID, seeds = [BLOCKLIST_SEED, mint.key().as_ref()], bump)]
    pub blocklist: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CloseBlocklist<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: parsed in the handler when the list is sealed (the mint exists then)
    pub mint: UncheckedAccount<'info>,
    /// CHECK: checked in the handler
    #[account(mut, owner = crate::ID, seeds = [BLOCKLIST_SEED, mint.key().as_ref()], bump)]
    pub blocklist: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct Execute<'info> {
    /// CHECK: token account, parsed in the handler
    pub source_token: UncheckedAccount<'info>,
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: token account, parsed in the handler
    pub destination_token: UncheckedAccount<'info>,
    /// CHECK: transfer authority, unused
    pub owner: UncheckedAccount<'info>,
    /// CHECK: validated by the token program
    #[account(seeds = [b"extra-account-metas", mint.key().as_ref()], bump)]
    pub extra_account_meta_list: UncheckedAccount<'info>,
    #[account(seeds = [LAUNCH_SEED, mint.key().as_ref()], bump = launch.bump)]
    pub launch: Account<'info, Launch>,
    /// CHECK: the blocklist PDA from the extra-account-meta list; may be empty
    pub blocklist: UncheckedAccount<'info>,
}

#[error_code]
pub enum HookError {
    #[msg("Mint does not use this transfer hook")]
    WrongHookProgram,
    #[msg("Launch account does not match the mint")]
    BadLaunch,
    #[msg("Max hold: no wallet may hold more than this coin's cap")]
    MaxHold,
    #[msg("Max hold must be between 0.5% and 4% of the supply")]
    BadMaxHold,
    #[msg("Sell lock must be 0 or between 10 seconds and 24 hours")]
    BadSellLock,
    #[msg("This quick-bond coin can't be sold until it bonds")]
    SellsLocked,
    #[msg("This coin can't be sent wallet to wallet until it bonds")]
    NoTransfers,
    #[msg("This wallet is blocked from buying this coin until it bonds")]
    Blocked,
    #[msg("Blocklist account is not valid for this mint")]
    BadBlocklist,
    #[msg("Only the wallet that opened the blocklist can change it or launch with it")]
    NotListOwner,
    #[msg("The blocklist is sealed: the coin has launched")]
    Sealed,
    #[msg("Blocklist wallets must be sent in ascending order with no repeats")]
    Unsorted,
    #[msg("Too many wallets on one blocklist")]
    TooManyBlocked,
    #[msg("The coin hasn't bonded yet, so its blocklist is still in force")]
    StillActive,
    #[msg("App rules are not valid")]
    BadAppRules,
    #[msg("This coin's launcher doesn't allow buys through this app")]
    AppNotAllowed,
}
