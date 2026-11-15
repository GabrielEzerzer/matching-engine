# Tests for the matching-engine layer rules: each rule fires on a sample that breaks the layering and stays quiet
# on one that follows it.
import unittest

from test_rules import scan_matches, setUpModule


def fired(files):
    return {(m["file"].replace("\\", "/").removeprefix("./"), m["ruleId"]) for m in scan_matches(files)}


class Layers(unittest.TestCase):
    def check(self, rule, bad, good):
        for name, text in bad:
            with self.subTest(rule=rule, bad=name):
                self.assertIn((name, rule), fired({name: text}))
        for name, text in good:
            with self.subTest(rule=rule, good=name):
                self.assertEqual(fired({name: text}), set())

    def test_library_no_io(self):
        self.check("layers-library-no-io", [
            ("src/a.rs", "use std::fs::File;\n"),
            ("src/b.rs", "use std::{collections::HashMap, io};\n"),
            ("src/c.rs", "fn now() -> u64 { std::time::Instant::now().elapsed().as_secs() }\n"),
            ("src/d.rs", "use std::env;\n"),
        ], [
            ("src/a.rs", "use std::collections::HashMap;\nuse std::fmt;\n"),
            ("examples/a.rs", "use std::fs::File;\nuse std::time::Instant;\n"),
            ("harness/a.rs", "use std::io::{self, Read};\n"),
        ])

    def test_library_no_includes(self):
        self.check("layers-library-no-includes", [
            ("src/a.rs", "#[path = \"../harness/reference.rs\"]\nmod reference;\n"),
            ("src/b.rs", "const DATA: &str = include_str!(\"../harness/golden/12302019.txt\");\n"),
        ], [
            ("src/lib.rs", "mod book;\npub mod itch;\n"),
        ])

    def test_book_no_feed(self):
        self.check("layers-book-no-feed", [
            ("src/book.rs", "use crate::itch::Message;\n"),
            ("src/side.rs", "use crate::{itch, level::Level};\n"),
            ("src/orders.rs", "fn f(r: crate::replay::Replay) {}\n"),
        ], [
            ("src/book.rs", "use crate::level::Nodes;\nuse crate::types::Side;\n"),
            ("src/replay.rs", "use crate::itch::{Locate, Message};\n"),
        ])

    def test_itch_standalone(self):
        self.check("layers-itch-standalone", [
            ("src/itch.rs", "use crate::types::Side;\n"),
            ("src/itch.rs", "mod helpers {\n    use crate::types::Side;\n}\n"),
        ], [
            ("src/itch.rs", "use std::fmt;\npub(crate) fn f() {}\nmod tests {\n    use super::*;\n}\n"),
        ])

    def test_types_leaf(self):
        self.check("layers-types-leaf", [
            ("src/types.rs", "use crate::level::Level;\n"),
        ], [
            ("src/types.rs", "pub type OrderId = u64;\npub(crate) type Slot = u32;\n"),
            ("src/book.rs", "use crate::types::Side;\n"),
        ])

    def test_includes_harness_only(self):
        self.check("layers-includes-harness-only", [
            ("tests/a.rs", "#[path = \"../src/book.rs\"]\nmod book;\n"),
            ("examples/a.rs", "#[path = \"../tests/prop.rs\"]\nmod prop;\n"),
            ("tests/b.rs", "const DATA: &[u8] = include_bytes!(\"../harness/golden/12302019.txt\");\n"),
        ], [
            ("tests/a.rs", "#[path = \"../harness/reference.rs\"]\nmod reference;\n"),
            ("examples/a.rs", "#[path = \"../harness/stream.rs\"]\nmod stream;\n"),
        ])

    def test_harness_no_includes(self):
        self.check("layers-harness-no-includes", [
            ("harness/a.rs", "#[path = \"../tests/prop.rs\"]\nmod prop;\n"),
            ("harness/b.rs", "#[path = \"stream.rs\"]\nmod stream;\n"),
        ], [
            ("harness/a.rs", "use matching_engine::OrderBook;\n"),
        ])

    def test_internals_for_microbenchmarks(self):
        self.check("layers-internals-for-microbenchmarks", [
            ("tests/prop.rs", "use matching_engine::internals::Level;\n"),
            ("harness/reference.rs", "use matching_engine::{OrderBook, internals::OrderMap};\n"),
            ("examples/bench.rs", "fn f(l: &matching_engine::internals::Level) {}\n"),
        ], [
            ("tests/workload.rs", "use matching_engine::internals::{OrderMap, RestingOrder};\n"),
            ("harness/workload.rs", "use matching_engine::internals::BookSide;\n"),
            ("examples/micro.rs", "use matching_engine::internals::Nodes;\n"),
            ("tests/prop.rs", "use matching_engine::{Command, OrderBook};\n"),
        ])

    def test_scripts_no_test_imports(self):
        self.check("layers-scripts-no-test-imports", [
            ("scripts/compare_summary.py", "import test_compare_summary\n"),
            ("scripts/other.py", "from test_compare_summary import comparison\n"),
            ("scripts/third.py", "import json, test_compare_summary\n"),
        ], [
            ("scripts/test_compare_summary.py", "import compare_summary as cs\n"),
            ("scripts/compare_summary.py", "import json\nimport math\n"),
        ])

    def test_kaggle_runner_single_file(self):
        self.check("layers-kaggle-runner-single-file", [
            ("scripts/kaggle_bench.py", "import compare_summary\n"),
            ("scripts/kaggle_bench.py", "from compare_summary import load\n"),
        ], [
            ("scripts/kaggle_bench.py", "import json\nfrom pathlib import Path\n"),
        ])


if __name__ == "__main__":
    unittest.main()
