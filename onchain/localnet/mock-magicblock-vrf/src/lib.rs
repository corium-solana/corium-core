//! Local-validator stand-in for MagicBlock `ephemeral-vrf`.
//!
//! Loaded at MagicBlock's real address with
//! `solana-test-validator --bpf-program Vrf1RN... mock_magicblock_vrf.so`, which
//! needs no keypair for the address. That matters: soldust hardcodes the VRF
//! program id and derives the callback identity underneath it, so a mock is only
//! useful if it sits at the real address.
//!
//! Ported from the ORAO mock this replaces. The shape of the job changed
//! completely with the provider. ORAO was pull-based, so that mock's fidelity
//! was all about *account layout* - soldust read ORAO's account and parsed bytes
//! out of it at fixed offsets. MagicBlock is push-based, so nothing here needs a
//! faithful account layout at all; what has to be faithful is the *authority
//! model*, because that is now the whole of soldust's safety argument:
//!
//! * a request is only accepted from a signer of `PDA([b"identity"], callback)`,
//!   which only the callback program itself can produce. This is what makes it
//!   impossible for an outsider to aim a callback at one of our rounds, and it
//!   is modelled exactly rather than waved through.
//! * a fulfilment invokes the callback program with
//!   `PDA([b"identity", callback], vrf)` as a *CPI* signer at account 0, which
//!   only this program can produce.
//! * the callback's accounts, discriminator and args are read back out of the
//!   stored request, not taken from whoever calls `mock_fulfill`. The real
//!   program does the same - it reads them off the queue item - and it is the
//!   reason an oracle cannot redirect a draw to a round that did not ask.
//! * instruction data is `disc ++ randomness(32) ++ args`, and the randomness is
//!   32 bytes, matching `provide_randomness.rs`.
//! * fulfilment is refused in the request's own slot, as the real program does.
//!
//! Wire formats are transcribed from `magicblock-labs/ephemeral-vrf`
//! (`program/src/request_randomness.rs`, `program/src/provide_randomness.rs`).
//!
//! Deliberate divergences, none of which soldust can observe:
//!
//! * No VRF proof. `mock_fulfill` takes the draw as an argument, because the
//!   point of the harness is to control *what* lands and *when*, not to model
//!   elliptic-curve verification.
//! * No queue account internals. The real queue is a VRF-owned account holding a
//!   packed item list; here the fee is simply transferred into the pinned queue
//!   address and each request is parked in its own PDA. Soldust only ever pins
//!   the queue's address and pays into it, so it cannot tell the difference.
//! * `mock_fulfill_unpinned` has no counterpart at all - see its comment.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    account_info::next_account_info,
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    system_instruction,
};

/// MagicBlock's real program id, on devnet and mainnet-beta alike.
const MAGICBLOCK_VRF_PROGRAM_ID: Pubkey =
    pubkey!("Vrf1RNUjXmQGjmQrQLvJHs9SNkvDJEsRVFPkfSQUwGz");

/// `ephemeral_vrf_api::consts::IDENTITY`.
const IDENTITY: &[u8] = b"identity";

/// `ephemeral_vrf_api::consts::VRF_LAMPORTS_COST`.
const VRF_LAMPORTS_COST: u64 = 500_000;

/// `SolanaVrfInstruction::RequestRandomnessScoped`, a one-byte tag padded to
/// eight. The only real instruction soldust sends.
const IX_REQUEST_RANDOMNESS_SCOPED: [u8; 8] = [10, 0, 0, 0, 0, 0, 0, 0];

/// Harness-only tags. High bytes so they cannot collide with a real
/// `SolanaVrfInstruction` tag, which is a small integer in the low byte.
const IX_MOCK_FULFILL: [u8; 8] = [0xF0, 0, 0, 0, 0, 0, 0, 0];
const IX_MOCK_FULFILL_UNPINNED: [u8; 8] = [0xF1, 0, 0, 0, 0, 0, 0, 0];

/// Requests live in the queue account, as they do in the real program, because
/// soldust's CPI passes exactly the five accounts the real instruction takes -
/// there is no room to hand the mock a scratch account of its own, and adding
/// one would mean soldust sending a request the real program would reject.
///
/// The real queue packs items variable-length and resizes; fixed slots here
/// keep the mock free of resize logic. `validator.sh` pre-loads the queue at
/// its pinned address, owned by this program, since a PDA of the real program
/// cannot be created by a mock standing in for it.
const QUEUE_HEADER: usize = 8;

/// Fixed slab per request. Far more than soldust's one-account, no-args request
/// needs.
const ITEM_SIZE: usize = 512;

const MAX_METAS: usize = 8;
const META_SIZE: usize = 34; // pubkey 32 ++ is_signer 1 ++ is_writable 1

// Item layout within a queue slot. Private to the mock.
const OFF_USED: usize = 0;
const OFF_CALLBACK_PROGRAM: usize = 1;
const OFF_SEED: usize = 33;
const OFF_SLOT: usize = 65;
const OFF_IDENTITY_BUMP: usize = 73;
const OFF_DISC_LEN: usize = 74;
const OFF_DISC: usize = 75;
const OFF_METAS_LEN: usize = 83;
const OFF_METAS: usize = 84;
const OFF_ARGS_LEN: usize = OFF_METAS + MAX_METAS * META_SIZE; // 356
const OFF_ARGS: usize = OFF_ARGS_LEN + 2; // 358

entrypoint!(process_instruction);

fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if program_id != &MAGICBLOCK_VRF_PROGRAM_ID {
        msg!("mock vrf loaded at the wrong address: {}", program_id);
        return Err(ProgramError::IncorrectProgramId);
    }
    if data.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let (tag, body) = data.split_at(8);
    match <[u8; 8]>::try_from(tag).unwrap() {
        IX_REQUEST_RANDOMNESS_SCOPED => request_randomness_scoped(accounts, body),
        IX_MOCK_FULFILL => mock_fulfill(accounts, body),
        IX_MOCK_FULFILL_UNPINNED => mock_fulfill_unpinned(accounts, body),
        other => {
            msg!("mock vrf: unhandled tag {:?}", other);
            Err(ProgramError::InvalidInstructionData)
        }
    }
}

/// Borsh cursor. Hand-rolled so the mock stays dependency-light and so the wire
/// format it accepts is spelled out rather than derived.
struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn take(&mut self, n: usize) -> core::result::Result<&'a [u8], ProgramError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(ProgramError::InvalidInstructionData)?;
        let out = self
            .data
            .get(self.at..end)
            .ok_or(ProgramError::InvalidInstructionData)?;
        self.at = end;
        Ok(out)
    }

    fn array32(&mut self) -> core::result::Result<[u8; 32], ProgramError> {
        Ok(<[u8; 32]>::try_from(self.take(32)?).unwrap())
    }

    fn u32(&mut self) -> core::result::Result<u32, ProgramError> {
        Ok(u32::from_le_bytes(<[u8; 4]>::try_from(self.take(4)?).unwrap()))
    }
}

/// `RequestRandomnessScoped`, the one instruction soldust CPIs into.
///
/// Accounts, in the order `ephemeral-vrf` documents and soldust builds:
///   0. `[signer, writable]` payer
///   1. `[signer]`           program identity, `PDA([b"identity"], callback)`
///   2. `[writable]`         oracle queue
///   3. `[]`                 system program
///   4. `[]`                 SlotHashes sysvar
///
/// Exactly these five, no more: soldust builds the same metas the real
/// instruction documents, so anything extra here would mean soldust is sending
/// a request the real program would refuse.
fn request_randomness_scoped(accounts: &[AccountInfo], body: &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let payer = next_account_info(it)?;
    let identity = next_account_info(it)?;
    let queue = next_account_info(it)?;
    let system_program = next_account_info(it)?;
    let _slot_hashes = next_account_info(it)?;

    if !payer.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !queue.is_writable {
        return Err(ProgramError::InvalidArgument);
    }
    // The real program requires this too. It is how soldust's queue pin earns
    // its keep: a queue the caller controls would not be owned by the oracle.
    if queue.owner != &MAGICBLOCK_VRF_PROGRAM_ID {
        msg!("mock vrf: queue is not owned by the VRF program");
        return Err(ProgramError::IllegalOwner);
    }

    //   caller_seed             [u8; 32]
    //   callback_program_id     Pubkey
    //   callback_discriminator  Vec<u8>
    //   callback_accounts_metas Vec<SerializableAccountMeta>
    //   callback_args           Vec<u8>
    let mut c = Cursor::new(body);
    let seed = c.array32()?;
    let callback_program_id = Pubkey::new_from_array(c.array32()?);

    let disc_len = c.u32()? as usize;
    // The real program rejects anything longer than 8 (`ArgumentSizeTooLarge`).
    if disc_len > 8 {
        msg!("mock vrf: callback discriminator longer than 8 bytes");
        return Err(ProgramError::InvalidInstructionData);
    }
    let disc = c.take(disc_len)?.to_vec();

    let metas_len = c.u32()? as usize;
    if metas_len > MAX_METAS {
        msg!("mock vrf: more callback accounts than this mock parks");
        return Err(ProgramError::InvalidInstructionData);
    }
    let metas = c.take(metas_len * META_SIZE)?.to_vec();

    let args_len = c.u32()? as usize;
    if OFF_ARGS + args_len > ITEM_SIZE {
        msg!("mock vrf: callback args longer than this mock parks");
        return Err(ProgramError::InvalidInstructionData);
    }
    let args = c.take(args_len)?.to_vec();

    // THE check. `ephemeral-vrf` requires the identity to both derive from
    // `[b"identity"]` under the *callback program* and to have signed, so only
    // that program can file a request naming itself. Without this a stranger
    // could aim a draw at any of soldust's rounds, so the mock enforces it
    // exactly rather than trusting soldust to have got its side right.
    let (expected_identity, _) =
        Pubkey::find_program_address(&[IDENTITY], &callback_program_id);
    if identity.key != &expected_identity {
        msg!(
            "mock vrf: identity {} is not PDA([identity], {})",
            identity.key,
            callback_program_id
        );
        return Err(ProgramError::InvalidSeeds);
    }
    if !identity.is_signer {
        msg!("mock vrf: the callback program's identity did not sign");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // The bump is precomputed at request time by the real program too, so
    // fulfilment can use the cheap `create_program_address`.
    let (_, identity_bump) = Pubkey::find_program_address(
        &[IDENTITY, callback_program_id.as_ref()],
        &MAGICBLOCK_VRF_PROGRAM_ID,
    );

    {
        let mut q = queue.try_borrow_mut_data()?;
        // A seed already in flight is refused, as the real queue refuses a
        // duplicate request id.
        if find_slot(&q, |item| item[OFF_USED] != 0 && item[OFF_SEED..OFF_SEED + 32] == seed)
            .is_some()
        {
            msg!("mock vrf: a request with this seed is already queued");
            return Err(ProgramError::InvalidArgument);
        }
        let at = find_slot(&q, |item| item[OFF_USED] == 0)
            .ok_or(ProgramError::AccountDataTooSmall)?;
        let d = &mut q[at..at + ITEM_SIZE];

        d[OFF_USED] = 1;
        d[OFF_CALLBACK_PROGRAM..OFF_CALLBACK_PROGRAM + 32]
            .copy_from_slice(callback_program_id.as_ref());
        d[OFF_SEED..OFF_SEED + 32].copy_from_slice(&seed);
        d[OFF_SLOT..OFF_SLOT + 8].copy_from_slice(&Clock::get()?.slot.to_le_bytes());
        d[OFF_IDENTITY_BUMP] = identity_bump;
        d[OFF_DISC_LEN] = disc_len as u8;
        d[OFF_DISC..OFF_DISC + disc_len].copy_from_slice(&disc);
        d[OFF_METAS_LEN] = metas_len as u8;
        d[OFF_METAS..OFF_METAS + metas.len()].copy_from_slice(&metas);
        d[OFF_ARGS_LEN..OFF_ARGS_LEN + 2].copy_from_slice(&(args_len as u16).to_le_bytes());
        d[OFF_ARGS..OFF_ARGS + args_len].copy_from_slice(&args);
    }

    // The requester pays into the queue; the real program later pays the oracle
    // back out of it at fulfilment. Soldust measures its outlay across the CPI,
    // so all it can observe is that this left the payer.
    invoke(
        &system_instruction::transfer(payer.key, queue.key, VRF_LAMPORTS_COST),
        &[payer.clone(), queue.clone(), system_program.clone()],
    )?;

    msg!(
        "mock vrf: parked request seed={} callback={} fee={}",
        short(&seed),
        callback_program_id,
        VRF_LAMPORTS_COST
    );
    Ok(())
}

/// What the oracle network does off-chain, then submits: land the draw.
///
/// Anyone may call this on localnet. That is not a shortcut around the security
/// model - the model does not care who *asks* for a fulfilment, only that the
/// callback carries a signature for `PDA([b"identity", callback], vrf)`, which
/// no caller can produce and which is applied below.
///
/// Accounts:
///   0. `[writable]` the oracle queue
///   1. `[]`         the callback program
///   2. `[]`         the scoped callback identity
///   3.. `[varies]`  the accounts the request named, in order
///
/// Args: `seed: [u8; 32] ++ randomness: [u8; 32]`.
fn mock_fulfill(accounts: &[AccountInfo], body: &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let queue = next_account_info(it)?;
    let callback_program = next_account_info(it)?;
    let identity = next_account_info(it)?;
    let rest: Vec<AccountInfo> = it.cloned().collect();

    if queue.owner != &MAGICBLOCK_VRF_PROGRAM_ID {
        return Err(ProgramError::IllegalOwner);
    }
    let seed =
        <[u8; 32]>::try_from(body.get(..32).ok_or(ProgramError::InvalidInstructionData)?)
            .unwrap();
    let randomness =
        <[u8; 32]>::try_from(body.get(32..64).ok_or(ProgramError::InvalidInstructionData)?)
            .unwrap();

    let (callback_program_id, identity_bump, disc, metas, args) = {
        let mut q = queue.try_borrow_mut_data()?;
        let at = find_slot(&q, |item| {
            item[OFF_USED] != 0 && item[OFF_SEED..OFF_SEED + 32] == seed
        })
        .ok_or_else(|| {
            msg!("mock vrf: no queued request for that seed");
            ProgramError::InvalidArgument
        })?;
        let d = &mut q[at..at + ITEM_SIZE];

        // The real program refuses to fulfil in the request's own slot, so a
        // scenario that tries to seal and draw atomically has to discover that
        // here rather than in production.
        let slot = u64::from_le_bytes(<[u8; 8]>::try_from(&d[OFF_SLOT..OFF_SLOT + 8]).unwrap());
        if Clock::get()?.slot <= slot {
            msg!("mock vrf: fulfilment must land in a later slot than the request");
            return Err(ProgramError::InvalidArgument);
        }

        let callback_program_id = Pubkey::new_from_array(
            <[u8; 32]>::try_from(&d[OFF_CALLBACK_PROGRAM..OFF_CALLBACK_PROGRAM + 32]).unwrap(),
        );
        let disc_len = d[OFF_DISC_LEN] as usize;
        let metas_len = d[OFF_METAS_LEN] as usize;
        let args_len = u16::from_le_bytes(
            <[u8; 2]>::try_from(&d[OFF_ARGS_LEN..OFF_ARGS_LEN + 2]).unwrap(),
        ) as usize;
        let out = (
            callback_program_id,
            d[OFF_IDENTITY_BUMP],
            d[OFF_DISC..OFF_DISC + disc_len].to_vec(),
            d[OFF_METAS..OFF_METAS + metas_len * META_SIZE].to_vec(),
            d[OFF_ARGS..OFF_ARGS + args_len].to_vec(),
        );
        // Removed before the callback runs, exactly as the real program does it,
        // so a replay has nothing left to find.
        d[OFF_USED] = 0;
        out
    };

    if callback_program.key != &callback_program_id {
        msg!("mock vrf: that is not the program this request named");
        return Err(ProgramError::IncorrectProgramId);
    }

    // Accounts come out of the stored request, never from the caller. This is
    // the property that stops an oracle pointing a draw at a round that did not
    // ask for one, so the mock reproduces it and checks the caller passed
    // exactly the accounts the request recorded.
    let mut metas_out = vec![AccountMeta {
        pubkey: *identity.key,
        is_signer: true,
        is_writable: false,
    }];
    for (i, chunk) in metas.chunks(META_SIZE).enumerate() {
        let pubkey = Pubkey::new_from_array(<[u8; 32]>::try_from(&chunk[..32]).unwrap());
        match rest.get(i) {
            Some(passed) if passed.key == &pubkey => {}
            _ => {
                msg!(
                    "mock vrf: account {} must be {}, as the request recorded",
                    i,
                    pubkey
                );
                return Err(ProgramError::InvalidArgument);
            }
        }
        metas_out.push(AccountMeta {
            pubkey,
            is_signer: chunk[32] != 0,
            is_writable: chunk[33] != 0,
        });
    }

    invoke_callback(
        callback_program,
        callback_program_id,
        identity,
        identity_bump,
        metas_out,
        &rest,
        disc,
        randomness,
        args,
    )
}

/// A VRF program that has stopped honouring its own queue: fulfil with a
/// caller-chosen callback account set, ignoring what the request recorded.
///
/// This models the residual trust assumption rather than a reachable attack.
/// The real program reads its accounts off the queue item, so an *outsider* can
/// never do this; only MagicBlock could, by shipping a program upgrade. It
/// exists so the suite can state on the record what a compromised oracle can
/// and cannot do to soldust - see the cross-round scenario.
///
/// Accounts:
///   0. `[]`         the callback program
///   1. `[]`         the scoped callback identity
///   2.. `[varies]`  whatever the caller wants the callback to touch
///
/// Args: `randomness: [u8; 32] ++ disc: [u8; 8] ++ args: [..]`.
fn mock_fulfill_unpinned(accounts: &[AccountInfo], body: &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let callback_program = next_account_info(it)?;
    let identity = next_account_info(it)?;
    let rest: Vec<AccountInfo> = it.cloned().collect();

    let randomness =
        <[u8; 32]>::try_from(body.get(..32).ok_or(ProgramError::InvalidInstructionData)?)
            .unwrap();
    let disc = body
        .get(32..40)
        .ok_or(ProgramError::InvalidInstructionData)?
        .to_vec();
    let args = body.get(40..).unwrap_or(&[]).to_vec();

    let callback_program_id = *callback_program.key;
    let (expected, identity_bump) = Pubkey::find_program_address(
        &[IDENTITY, callback_program_id.as_ref()],
        &MAGICBLOCK_VRF_PROGRAM_ID,
    );
    if identity.key != &expected {
        return Err(ProgramError::InvalidSeeds);
    }

    let mut metas_out = vec![AccountMeta {
        pubkey: *identity.key,
        is_signer: true,
        is_writable: false,
    }];
    for a in &rest {
        metas_out.push(AccountMeta {
            pubkey: *a.key,
            is_signer: false,
            is_writable: a.is_writable,
        });
    }

    msg!("mock vrf: UNPINNED fulfilment - modelling a rogue oracle program");
    invoke_callback(
        callback_program,
        callback_program_id,
        identity,
        identity_bump,
        metas_out,
        &rest,
        disc,
        randomness,
        args,
    )
}

/// Build and sign the callback exactly as `provide_randomness` does:
/// `disc ++ randomness(32) ++ args`, with the scoped identity as CPI signer at
/// account 0.
#[allow(clippy::too_many_arguments)]
fn invoke_callback<'a>(
    callback_program: &AccountInfo<'a>,
    callback_program_id: Pubkey,
    identity: &AccountInfo<'a>,
    identity_bump: u8,
    metas: Vec<AccountMeta>,
    rest: &[AccountInfo<'a>],
    disc: Vec<u8>,
    randomness: [u8; 32],
    args: Vec<u8>,
) -> ProgramResult {
    let mut data = Vec::with_capacity(disc.len() + 32 + args.len());
    data.extend_from_slice(&disc);
    data.extend_from_slice(&randomness);
    data.extend_from_slice(&args);

    let ix = Instruction {
        program_id: callback_program_id,
        accounts: metas,
        data,
    };

    let mut infos = vec![callback_program.clone(), identity.clone()];
    infos.extend_from_slice(rest);

    let bump = [identity_bump];
    let seeds: &[&[u8]] = &[IDENTITY, callback_program_id.as_ref(), &bump];
    invoke_signed(&ix, &infos, &[seeds])?;
    msg!("mock vrf: delivered draw {}", short(&randomness));
    Ok(())
}

/// Byte offset of the first queue slot matching `pred`, if any.
fn find_slot(queue: &[u8], pred: impl Fn(&[u8]) -> bool) -> Option<usize> {
    let slots = queue.len().saturating_sub(QUEUE_HEADER) / ITEM_SIZE;
    (0..slots)
        .map(|i| QUEUE_HEADER + i * ITEM_SIZE)
        .find(|&at| pred(&queue[at..at + ITEM_SIZE]))
}

fn short(bytes: &[u8]) -> String {
    bytes.iter().take(4).map(|b| format!("{b:02x}")).collect()
}
