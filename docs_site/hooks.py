"""MkDocs hook: regenerate docs-site/*.md from docs/wiki/ before each build.

``docs_site.convert`` drops ``<!-- wiki-only -->`` blocks, so a note can stay on the
GitHub wiki and off this site. ``_Sidebar.md`` and ``_Footer.md`` are not copied.
"""

from __future__ import annotations

import importlib
import logging
import sys
from pathlib import Path

log = logging.getLogger("mkdocs")
ROOT = Path(__file__).resolve().parents[1]


def _convert():
    """Reload so `mkdocs serve` picks up edits. The hook module itself stays loaded."""
    if str(ROOT) not in sys.path:
        sys.path.insert(0, str(ROOT))
    import docs_site.convert as convert

    return importlib.reload(convert)


def on_config(config):
    config.mdx_configs.setdefault("toc", {})["slugify"] = _convert().github_slugify
    return config


def on_pre_build(config) -> None:
    wiki = Path(config.config_file_path).resolve().parent / "docs" / "wiki"
    count = _convert().generate(Path(config.docs_dir), wiki)
    log.info("Wrote %s docs pages from %s", count, wiki)
