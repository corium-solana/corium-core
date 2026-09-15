//! Checked arithmetic helpers. `overflow-checks = true` is also on in
//! `Cargo.toml`, but explicit checks give a typed error instead of a panic.

use anchor_lang::prelude::*;

use crate::errors::SoldustError;

#[inline]
pub fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| SoldustError::MathOverflow.into())
}

#[inline]
pub fn sub(a: u64, b: u64) -> Result<u64> {
    a.checked_sub(b)
        .ok_or_else(|| SoldustError::MathOverflow.into())
}
