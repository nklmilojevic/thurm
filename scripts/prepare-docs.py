#!/usr/bin/env python3
"""Create the configuration reference from the source example."""
from pathlib import Path

root = Path(__file__).resolve().parent.parent
example = (root / "config.example.toml").read_text()
(root / "docs/configuration-reference.md").write_text(
    "# Configuration reference\n\n"
    "This reference comes from `config.example.toml`. Active values are defaults.\n"
    "Commented settings are optional examples. All settings are optional.\n\n"
    "See [Configuration](configuration.md) for the file path and reload steps.\n\n"
    "```toml\n" + example + "```\n"
)
