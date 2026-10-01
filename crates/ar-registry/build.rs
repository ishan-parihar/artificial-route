//! Stamps the build so the compiled-in catalog can be dated.
//!
//! `registry.json` is a generated snapshot with no date in it — a top-level
//! metadata key would break the `BTreeMap<Strng, ProviderDef>` shape every loader
//! parses, and would put a changing line in a file whose byte-stability is what
//! makes a regenerated catalog reviewable in a diff. So the only clock available
//! to a runtime reading the catalog is the build's own, and this is where it is
//! taken.
//!
//! The fact recorded is *when this binary was linked*, not when the upstream
//! snapshot was fetched. That is the conservative direction for a staleness
//! warning: a snapshot can never be older than the binary carrying it, so
//! "the build is N days old" under-claims the drift rather than over-claiming it.
//!
//! Std-only, so the date costs no new dependency.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // `cargo:rerun-if-changed` is deliberately not set on `registry.json`: the
    // stamp is the build's, and re-running this script on every catalog edit
    // would date the *build* by the last time someone touched the snapshot, which
    // is the conflation this file exists to avoid.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    println!("cargo:rustc-env=AR_REGISTRY_BUILT_AT={secs}");
}
