#!/usr/bin/env python3
"""Regression tests for the read-only capability detector."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from detect_project_capabilities import detect


class CapabilityDetectorTests(unittest.TestCase):
    def test_dependency_without_initialization_is_partial(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "package.json").write_text(
                '{"dependencies":{"@sentry/nextjs":"1.0.0"}}', encoding="utf-8"
            )

            result = detect(root)

            self.assertEqual(result["capabilities"]["error_tracking"]["status"], "partial")

    def test_source_initialization_is_configured(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "package.json").write_text(
                '{"dependencies":{"@opentelemetry/sdk-node":"1.0.0"}}', encoding="utf-8"
            )
            (root / "instrumentation.ts").write_text(
                "const sdk = new NodeSDK({}); sdk.start();", encoding="utf-8"
            )

            result = detect(root)

            self.assertEqual(result["capabilities"]["tracing"]["status"], "configured")

    def test_environment_placeholder_is_not_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "compose.yaml").write_text(
                "environment:\n  SENTRY_DSN: ${SENTRY_DSN}\n  OTEL_EXPORTER_OTLP_ENDPOINT: ${OTEL_ENDPOINT}\n",
                encoding="utf-8",
            )

            result = detect(root)

            self.assertEqual(result["capabilities"]["error_tracking"]["status"], "missing")
            self.assertEqual(result["capabilities"]["tracing"]["status"], "missing")

    def test_dotnet_project_is_detected_by_suffix(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Example.csproj").write_text("<Project />", encoding="utf-8")

            result = detect(root)

            self.assertIn("dotnet", [item["name"] for item in result["frameworks"]])


if __name__ == "__main__":
    unittest.main()
