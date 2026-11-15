//! The feature map in features.json stays complete: every entry point is mapped, and every mapped file exists.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::Value;

const FIELDS: [&str; 7] = [
    "name",
    "description",
    "command",
    "entry_point",
    "inputs",
    "outputs",
    "test",
];
const ORDER_BOOK_MODULE: &str = "src/book.rs";
const HELP_TIMEOUT: Duration = Duration::from_secs(60);

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn features() -> Vec<Value> {
    let text = fs::read_to_string(root().join("features.json")).expect("features.json exists");
    let map: Value = serde_json::from_str(&text).expect("features.json is valid JSON");
    map["features"]
        .as_array()
        .expect("features.json has a \"features\" list")
        .clone()
}

fn field<'a>(entry: &'a Value, key: &str) -> &'a str {
    entry[key].as_str().unwrap_or_else(|| {
        panic!("feature {entry} needs a non-empty string \"{key}\"; see the other entries in features.json")
    })
}

fn files_in(folder: &str, keep: impl Fn(&str) -> bool) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(root().join(folder))
        .unwrap_or_else(|e| panic!("{folder} is readable: {e}"))
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .into_string()
                .expect("UTF-8 name")
        })
        .filter(|name| keep(name))
        .map(|name| format!("{folder}/{name}"))
        .collect();
    found.sort();
    found
}

fn entry_points() -> BTreeSet<String> {
    let lib = fs::read_to_string(root().join("src/lib.rs")).expect("src/lib.rs exists");
    let modules = lib.lines().filter_map(|line| {
        let name = line.trim().strip_prefix("pub mod ")?.strip_suffix(';')?;
        Some(format!("src/{name}.rs"))
    });
    let examples = files_in("examples", |name| name.ends_with(".rs"));
    let scripts = files_in("scripts", |name| {
        name.ends_with(".sh") || (name.ends_with(".py") && !name.starts_with("test_"))
    });
    modules
        .chain([ORDER_BOOK_MODULE.to_string()])
        .chain(examples)
        .chain(scripts)
        .collect()
}

fn unmapped(points: &BTreeSet<String>, map: &[Value]) -> Vec<String> {
    let mapped: BTreeSet<&str> = map
        .iter()
        .map(|entry| field(entry, "entry_point"))
        .collect();
    points
        .iter()
        .filter(|point| !mapped.contains(point.as_str()))
        .cloned()
        .collect()
}

fn python() -> &'static str {
    ["python3", "python"]
        .into_iter()
        .find(|name| {
            Command::new(name)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
        .expect("Python 3 is on PATH as python3 or python; the feature map runs each Python entry point's --help")
}

fn exit_status_within(command: &mut Command, limit: Duration) -> Result<bool, String> {
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(status.success());
        }
        if start.elapsed() > limit {
            child.kill().ok();
            child.wait().ok();
            return Err(format!("still running after {limit:?}"));
        }
        sleep(Duration::from_millis(50));
    }
}

#[test]
fn every_entry_has_its_fields() {
    for entry in features() {
        for key in FIELDS {
            assert!(
                !field(&entry, key).trim().is_empty(),
                "feature {entry} has an empty \"{key}\""
            );
        }
        if field(&entry, "test") == "none" {
            assert!(
                entry["debt_issue"].as_u64().is_some_and(|n| n > 0),
                "feature \"{}\" has no test, so it needs \"debt_issue\": the number of the debt-labelled \
                 GitHub issue that tracks the missing test",
                field(&entry, "name")
            );
        }
    }
}

#[test]
fn every_entry_point_is_mapped() {
    let missing = unmapped(&entry_points(), &features());
    assert!(
        missing.is_empty(),
        "{missing:?} are entry points with no entry in features.json. Add one entry per user-facing capability \
         with its name, description, command, entry_point, inputs, outputs and test."
    );
}

#[test]
fn mapped_files_exist() {
    for entry in features() {
        let name = field(&entry, "name");
        let entry_point = field(&entry, "entry_point");
        assert!(
            root().join(entry_point).is_file(),
            "feature \"{name}\" maps to {entry_point}, which does not exist. Point entry_point at the module, example or script that runs it."
        );
        let test = field(&entry, "test");
        assert!(
            test == "none" || root().join(test).is_file(),
            "feature \"{name}\" names the test {test}, which does not exist. Name the test file that covers it, or \"none\" with a debt_issue."
        );
    }
}

#[test]
fn python_entry_points_answer_help() {
    let python = python();
    for entry in features() {
        let entry_point = field(&entry, "entry_point");
        if !entry_point.ends_with(".py") {
            continue;
        }
        let result = exit_status_within(
            Command::new(python)
                .arg(root().join(entry_point))
                .arg("--help"),
            HELP_TIMEOUT,
        );
        assert_eq!(
            result,
            Ok(true),
            "{python} {entry_point} --help must exit 0 within {HELP_TIMEOUT:?}; fix the script's argument parser"
        );
    }
}

#[test]
fn unmapped_names_a_missing_entry_point() {
    let points: BTreeSet<String> = ["src/book.rs", "examples/replay.rs"]
        .map(String::from)
        .into();
    let map = [serde_json::json!({ "entry_point": "src/book.rs" })];
    assert_eq!(unmapped(&points, &map), ["examples/replay.rs"]);
}
