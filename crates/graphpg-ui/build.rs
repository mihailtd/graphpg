use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=GRAPHPG_REQUIRE_UI");
    println!("cargo:rerun-if-changed=assets");

    let assets_dir = Path::new("assets/graphiql");
    let index_file = assets_dir.join("index.html");

    let has_assets = index_file.exists();
    let require_ui = env::var("GRAPHPG_REQUIRE_UI").as_deref() == Ok("1");

    if !has_assets {
        if require_ui {
            panic!(
                "GRAPHPG_REQUIRE_UI=1 is set, but UI assets were not found in crates/graphpg-ui/assets/! \
                 Run 'moon run graphiql-ui:build' or 'pnpm --filter @graphpg/ui-graphiql build' first."
            );
        }

        println!(
            "cargo:warning=graphpg-ui: UI assets not found in assets/graphiql. \
             Generating stub assets for compilation. Run 'moon run graphiql-ui:build' to build real assets."
        );

        fs::create_dir_all(assets_dir).expect("failed to create assets/graphiql stub directory");

        let stub_html = r#"<!doctype html>
<html lang="en">
  <head><title>graphpg · GraphiQL (stub)</title></head>
  <body><p>GraphiQL UI stub. Run <code>moon run graphiql-ui:build</code> to build assets.</p></body>
</html>"#;
        fs::write(&index_file, stub_html).expect("failed to write stub index.html");

        let stub_manifest = r#"{"versions":{},"files":{}}"#;
        fs::write(assets_dir.join("manifest.json"), stub_manifest).ok();
    }
}
