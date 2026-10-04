//! Looks after the one thing `rust-embed` cannot: telling a build without the UI from a build
//! with it. The assets are produced by `moon run graphiql-ui:build` and are not in git; a
//! build that lacks them still compiles (the page then explains what is missing), unless
//! `GRAPHPG_REQUIRE_UI=1`, which release builds set so that an image can never ship without them.
use std::env;
use std::path::Path;

/// Written last by the UI build, so its presence means the assets are complete.
const MANIFEST: &str = "assets/graphiql/manifest.json.gz";

fn main() {
    println!("cargo:rerun-if-env-changed=GRAPHPG_REQUIRE_UI");
    println!("cargo:rerun-if-changed=assets");

    if env::var_os("CARGO_FEATURE_GRAPHIQL").is_none() || Path::new(MANIFEST).exists() {
        return;
    }
    if env::var("GRAPHPG_REQUIRE_UI").as_deref() == Ok("1") {
        panic!(
            "GRAPHPG_REQUIRE_UI=1, but the UI assets are missing ({MANIFEST}). \
             Run `moon run graphiql-ui:build` first."
        );
    }
    println!(
        "cargo:warning=graphpg-ui: no GraphiQL assets in crates/graphpg-ui/assets, so /graphiql \
         will explain that. Run `moon run graphiql-ui:build` to build them."
    );
}
