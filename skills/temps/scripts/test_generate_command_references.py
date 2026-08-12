#!/usr/bin/env python3
"""Regression tests for the command-reference splitter."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from generate_command_references import split_catalog


class CommandReferenceGeneratorTests(unittest.TestCase):
    def test_splits_groups_and_excludes_monolithic_footer(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "COMMANDS.md"
            destination = root / "commands"
            source.write_text(
                "# CLI\n\n## `alpha`\n\nAlpha.\n\n## `beta`\n\nBeta.\n\n"
                "---\n\n## Examples\n\nNot a command group.\n",
                encoding="utf-8",
            )

            generated = split_catalog(source, destination)

            self.assertEqual([command for command, _ in generated], ["alpha", "beta"])
            self.assertNotIn("## Examples", (destination / "beta.md").read_text(encoding="utf-8"))
            self.assertIn("[`alpha`](alpha.md)", (destination / "INDEX.md").read_text(encoding="utf-8"))

    def test_adds_contents_to_a_long_group(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "COMMANDS.md"
            destination = root / "commands"
            body = "\n".join(f"line {number}" for number in range(301))
            source.write_text(
                f"# CLI\n\n## `services`\n\n### `services list`\n\n{body}\n",
                encoding="utf-8",
            )

            split_catalog(source, destination)

            output = (destination / "services.md").read_text(encoding="utf-8")
            self.assertIn("## Contents", output)
            self.assertIn("[`services list`](#services-list)", output)


if __name__ == "__main__":
    unittest.main()
