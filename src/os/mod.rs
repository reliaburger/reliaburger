//! The appliance operating system as a release artefact.
//!
//! Each weekly OS build is published as a GitHub release `os-<version>`, and
//! a signed `os-channel.json` names the newest one (`scripts/release/
//! os_release.py` writes both; see docs/plans/2026-10-01-plan-appliance-
//! product.md, W1). This module checks what a node or `relish` downloads:
//! the channel's signature against the release keys, then each
//! architecture's `SHA256SUMS` against the digest the channel names, then
//! every artefact against `SHA256SUMS`. So one signature vouches for the
//! whole release, and nothing is trusted on its name alone.

pub mod channel;
pub mod rollout;
pub mod slot;

pub use channel::{
    ChannelArch, OsChannel, OsError, OsVersion, SumsEntry, check_asset, signed_sums,
};
