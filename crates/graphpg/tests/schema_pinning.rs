//! Exporting the schemas (`graphpg schema export`), checking them in CI (`--check`) and the
//! pinned-schema check at startup, through the real binary.
mod common;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(name: &str, pin: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphpg-pin-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let project = Self { dir };
        let pin = if pin.is_empty() {
            String::new()
        } else {
            format!("  pin: {pin}\n")
        };
        std::fs::write(
            project.path("graphpg.yaml"),
            format!(
                "database:\n  schemas: [app_public]\nserver:\n  port: 0\nexport:\n  graphql: {d}/schema.graphql\n  openapi: {d}/openapi.json\n  subgraph: {d}/subgraph.graphql\n{pin}",
                d = project.dir.display()
            ),
        )
        .unwrap();
        project
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphpg"));
        command
            .args(["--config", self.path("graphpg.yaml").to_str().unwrap()])
            .args(args)
            .current_dir(&self.dir)
            .env("DATABASE_URL", common::db_url())
            .env_remove("GRAPHPG_MODE")
            .env_remove("RUST_LOG");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("graphpg runs")
    }

    /// Starts the server and gives it a few seconds: `Some(output)` if it exited by itself.
    fn start(&self, args: &[&str]) -> (Option<Output>, String) {
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("graphpg starts");
        for _ in 0..30 {
            if child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                let text = text_of(&output);
                return (Some(output), text);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        child.kill().ok();
        let output = child.wait_with_output().unwrap();
        (None, text_of(&output))
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn text_of(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn export_writes_every_schema_and_check_passes_until_something_changes() {
    common::block_on(common::ensure_forum_fixture());
    let project = Project::new("export", "");

    // nothing pinned yet: --check fails in CI (a schema nobody exported is not pinned)
    let before = project.run(&["schema", "export", "--check"]);
    assert!(!before.status.success(), "{}", text_of(&before));
    assert!(
        text_of(&before).contains("no pinned file"),
        "{}",
        text_of(&before)
    );

    let export = project.run(&["schema", "export"]);
    assert!(export.status.success(), "{}", text_of(&export));
    let sdl = read(&project.path("schema.graphql"));
    assert!(sdl.contains("type Query"), "{sdl}");
    let openapi: serde_json::Value =
        serde_json::from_str(&read(&project.path("openapi.json"))).unwrap();
    assert_eq!(openapi["openapi"], "3.1.0");
    assert!(openapi["paths"]["/rest/forums"].is_object());
    // the nested-write input types are in the document
    assert!(openapi["components"]["schemas"]["Input.CreateForumInput"].is_object());

    let check = project.run(&["schema", "export", "--check"]);
    assert!(check.status.success(), "{}", text_of(&check));
    assert!(text_of(&check).contains("up to date"));

    // a changed OpenAPI document and a changed SDL are both drift, and named
    let mut changed = openapi.clone();
    changed["paths"]["/rest/removed"] = serde_json::json!({});
    changed["paths"]
        .as_object_mut()
        .unwrap()
        .remove("/rest/forums");
    std::fs::write(
        project.path("openapi.json"),
        serde_json::to_string_pretty(&changed).unwrap(),
    )
    .unwrap();
    std::fs::write(
        project.path("schema.graphql"),
        sdl.replace("type Query", "type Qwery"),
    )
    .unwrap();
    let drift = project.run(&["schema", "export", "--check"]);
    assert!(!drift.status.success(), "{}", text_of(&drift));
    let report = text_of(&drift);
    assert!(
        report.contains("openapi") && report.contains("+ /paths/~1rest~1forums"),
        "{report}"
    );
    assert!(report.contains("- /paths/~1rest~1removed"), "{report}");
    assert!(
        report.contains("graphql")
            && report.contains("- type Qwery")
            && report.contains("+ type Query"),
        "{report}"
    );

    // exporting again repairs it
    assert!(project.run(&["schema", "export"]).status.success());
    assert!(project
        .run(&["schema", "export", "--check"])
        .status
        .success());

    // the OpenAPI document alone, as YAML
    let yaml = project.path("api.yaml");
    let out = project.run(&["export-openapi", "--out", yaml.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text_of(&out));
    assert!(read(&yaml).contains("openapi: 3.1.0"));
}

#[test]
fn startup_refuses_a_drifted_schema_in_prod_and_warns_in_dev() {
    common::block_on(common::ensure_forum_fixture());
    let project = Project::new("startup", "");
    assert!(project.run(&["schema", "export"]).status.success());

    // matching files: the server starts (and keeps running)
    let (exited, text) = project.start(&["--mode", "prod"]);
    assert!(
        exited.is_none(),
        "a matching pin starts the server:\n{text}"
    );

    // drifted: prod refuses, naming what changed
    let mut doc: serde_json::Value =
        serde_json::from_str(&read(&project.path("openapi.json"))).unwrap();
    doc["paths"]["/rest/stale"] = serde_json::json!({});
    std::fs::write(
        project.path("openapi.json"),
        serde_json::to_string_pretty(&doc).unwrap(),
    )
    .unwrap();
    let (exited, text) = project.start(&["--mode", "prod"]);
    let exited = exited.expect("prod does not start on drift");
    assert!(!exited.status.success());
    assert!(
        text.contains("export.pin: fail") && text.contains("- /paths/~1rest~1stale"),
        "{text}"
    );

    // dev warns and starts
    let (exited, text) = project.start(&["--mode", "dev"]);
    assert!(exited.is_none(), "dev starts despite drift:\n{text}");
    assert!(
        text.contains("does not match the database") && text.contains("/paths/~1rest~1stale"),
        "{text}"
    );
}

#[test]
fn the_pin_can_be_turned_off_or_made_a_warning_everywhere() {
    common::block_on(common::ensure_forum_fixture());
    for (pin, starts) in [("off", true), ("warn", true), ("fail", false)] {
        let project = Project::new(&format!("pin-{pin}"), pin);
        std::fs::write(
            project.path("schema.graphql"),
            "type Query { stale: Int }\n",
        )
        .unwrap();
        let (exited, text) = project.start(&["--mode", "prod"]);
        assert_eq!(exited.is_none(), starts, "pin: {pin}\n{text}");
    }
    // files that do not exist pin nothing
    let project = Project::new("nofiles", "fail");
    let (exited, text) = project.start(&["--mode", "prod"]);
    assert!(exited.is_none(), "{text}");
}
