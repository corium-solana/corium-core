use anchor_lang::prelude::*;

#[error_code]
pub enum SoldustError {
    #[msg("Arithmetic overflow")]
    MathOverflow,

    #[msg("The requested star is not the current star")]
    NotCurrentStar,

    #[msg("The star is not alive")]
    StarNotAlive,

    #[msg("The star is not dead yet")]
    StarNotDead,

    #[msg("The star is not a black hole")]
    StarNotBlackHole,

    #[msg("No hole ticket on this star")]
    NoFeedShare,

    #[msg("This hole share has already been claimed")]
    HoleShareAlreadyClaimed,

    #[msg("A star already exists; use create_next_star")]
    FirstStarAlreadyCreated,

    #[msg("The next star has already been created for this star")]
    NextStarAlreadyCreated,

    #[msg("Push amount must be greater than zero")]
    PushAmountOutOfRange,

    #[msg("Push amount must be an exact multiple of 0.01 SOL")]
    PushNotOnStep,

    #[msg("This push has already been resolved")]
    PushAlreadyResolved,

    #[msg("This push is still pending")]
    PushStillPending,

    #[msg("The push does not belong to the supplied star")]
    PushStarMismatch,

    #[msg("The supplied player account does not own this push")]
    PushPlayerMismatch,

    #[msg("The supplied randomness account does not match the push")]
    RandomnessAccountMismatch,

    #[msg("Randomness has not been fulfilled yet; retry later")]
    RandomnessNotReady,

    #[msg("The randomness account is not owned by the VRF program")]
    InvalidVrfAccountOwner,

    #[msg("The randomness account layout could not be parsed")]
    MalformedVrfAccount,

    #[msg("The randomness account seed does not match the push seed")]
    RandomnessSeedMismatch,

    #[msg("Incorrect VRF program supplied")]
    InvalidVrfProgram,

    #[msg("Incorrect ORAO network state account supplied")]
    InvalidVrfNetworkState,

    #[msg("Only the star killer may claim this prize")]
    NotStarKiller,

    #[msg("The prize for this star has already been claimed")]
    PrizeAlreadyClaimed,

    #[msg("Withdrawal would dip into reserved player funds")]
    InsufficientUnreservedFunds,

    #[msg("Requested amount exceeds accrued protocol fees")]
    ExceedsAccruedFees,

    #[msg("A live star must settle pushes in push_id order")]
    PushOutOfOrder,

    #[msg("This feed missed the nursery; SOL was not taken")]
    FeedWindowClosed,

    #[msg("Last-hit pushes start after the nursery is full")]
    LastHitNotOpen,

    #[msg("Randomness has already been requested for this round")]
    VrfAlreadyRequested,

    #[msg("This star has already committed its full mass")]
    StarClosed,

    #[msg("The previous star is still open")]
    StarStillOpen,

    // ------------------------------------------------------------ rounds
    #[msg("The supplied round is not the star's open round")]
    NotCurrentRound,

    #[msg("The push does not belong to the supplied round")]
    PushRoundMismatch,

    #[msg("This round is no longer accepting pushes")]
    RoundNotOpen,

    #[msg("This round has not been closed yet")]
    RoundNotClosed,

    #[msg("This round's draw has not been requested yet")]
    RoundNotRequested,

    #[msg("This round is still open for pushes or waiting on its draw")]
    RoundNotCloseable,

    #[msg("This round has not been stalled long enough to void")]
    RoundNotExpired,

    #[msg("This round has already been voided")]
    RoundExpired,

    #[msg("This round has no members")]
    RoundEmpty,

    #[msg("This round's stake does not cover a draw yet; it stays open for more members")]
    RoundBelowDrawCost,

    #[msg("This round still has unresolved members")]
    RoundNotDrained,

    #[msg("The SlotHashes sysvar account is missing or malformed")]
    InvalidSlotHashes,

    #[msg("That slot has aged out of SlotHashes; re-derive the seed and retry")]
    SlotHashTooOld,

    // ------------------------------------------------------------- deploy
    #[msg("Only the program's upgrade authority may initialize")]
    NotProgramAuthority,

    #[msg("The program data account is missing or malformed")]
    MalformedProgramData,

    // Appended rather than filed with the errors they belong beside so no
    // existing error code moved: Anchor numbers these by position, and clients
    // match on them.
    #[msg("That is not the treasury ORAO's own network state names")]
    InvalidVrfTreasury,

    #[msg("That would leave nothing to buy randomness with; the treasury must sign to take the float")]
    WouldDrainDrawFloat,

    #[msg("This star is still moving; it cannot be collapsed as stalled")]
    StarNotStalled,

    #[msg("Settle or refund every pending push before collapsing this star")]
    StarQueueNotEmpty,
}
