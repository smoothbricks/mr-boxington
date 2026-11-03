//! Which build produced a precompiled header's bytes.
//!
//! A Clang precompiled header records the absolute paths of the headers it was
//! built from, so the same header built in two checkouts has two sets of bytes,
//! and a compilation keyed by those bytes misses in every new checkout. A PCH
//! built under mbx is never served from the cache, since one from another
//! checkout would make the headers it reaches name that checkout's files. But
//! its build has a portable key like any compilation's: its normalized
//! arguments and the contents of the headers it read. The shim records, under
//! the cache, which build key produced which PCH bytes, and a compilation that
//! uses the PCH is keyed by that build instead of by the bytes. A PCH with no
//! record (built outside mbx, or before this record existed) keeps its bytes
//! in the key, which is the behavior without it.

use eyre::{Context, Result};
use mbx_cache_cc::CcActionContext;
use mbx_cache_core::CacheDigest;
use std::path::{Path, PathBuf};

/// Where the session tells the shim to keep the records.
pub(crate) const ROOT_ENV: &str = "MBX_PCH_IDENTITY_ROOT";

/// The cache-relative home of the records.
pub(crate) const ROOT: &str = "pch-identities/v1";

fn root() -> Option<PathBuf> {
    std::env::var_os(ROOT_ENV)
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
}

/// Record that the build keyed `build` produced the bytes now at `pch`. One
/// small file per set of bytes, named for their digest and written whole.
pub(crate) fn record(pch: &Path, build: &CacheDigest) -> Result<()> {
    let Some(root) = root() else {
        return Ok(());
    };
    let bytes = CacheDigest::blake3_file(pch)
        .wrap_err_with(|| format!("failed to digest {}", pch.display()))?;
    std::fs::create_dir_all(&root)
        .wrap_err_with(|| format!("failed to create {}", root.display()))?;
    let record = root.join(&bytes.hash);
    let staged = root.join(format!(".tmp-{}-{}", bytes.hash, std::process::id()));
    std::fs::write(&staged, serde_json::to_vec(build)?)
        .wrap_err_with(|| format!("failed to write {}", staged.display()))?;
    std::fs::rename(&staged, &record)
        .wrap_err_with(|| format!("failed to publish {}", record.display()))?;
    Ok(())
}

/// The build key recorded for a precompiled header with these bytes.
fn build_key(bytes: &CacheDigest) -> Option<CacheDigest> {
    let text = std::fs::read(root()?.join(&bytes.hash)).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Key each precompiled header among the compilation's inputs by the build
/// that produced its bytes, where one was recorded.
pub(crate) fn substitute(context: &mut CcActionContext) {
    for input in &mut context.inputs {
        let precompiled = input
            .path
            .extension()
            .is_some_and(|extension| extension == "pch" || extension == "gch");
        if precompiled && let Some(build) = build_key(&input.digest) {
            input.digest = build;
        }
    }
}
