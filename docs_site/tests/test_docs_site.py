"""The MkDocs site is built from docs/wiki/. The wiki files stay GitHub-flavored."""

import re
from pathlib import Path

import pytest
from conftest import ROOT
from test_wiki import _prose

from docs_site.convert import TITLES, convert_page, generate, github_slugify, slug_for

WIKI = ROOT / "docs" / "wiki"
LINK = re.compile(r"\]\(([^)\s]+)\)")
ITEM = re.compile(r"^ +- (.+): ([a-z0-9-]+)\.md\s*$", re.M)
GROUP = re.compile(r"^ +- ([^:\n]+):\s*$", re.M)
ADMONITION = re.compile(r'^!!! (\w+) "([^"]+)"$', re.M)
GROUPS = (
    "Start here",
    "Using Floki",
    "Working on Floki",
    "Design notes",
)


def _pages():
    return {path.stem: path.read_text(encoding="utf-8") for path in WIKI.glob("*.md")}


def test_slugify_matches_github_heading_ids():
    samples = [
        "OpenColorIO (#24)",
        "The walkthrough: `cargo ocio-run`",
        "The providers' own numbers",
        "1. State files and their locks (`src/app.rs`)",
        "2026-11-09: prompt v8 on Haiku",
        "Something broke?",
    ]
    for heading in samples:
        github = re.sub(r"[^\w\- ]", "", heading.strip().lower()).replace(" ", "-")
        assert github_slugify(heading, "-") == github
        assert github_slugify(heading.replace("`", ""), "-") == github


def test_titles_cover_every_wiki_page():
    pages = {stem for stem in _pages() if not stem.startswith("_")}
    assert pages == set(TITLES)
    assert len({slug_for(stem) for stem in pages}) == len(pages)


def test_nav_lists_every_page_in_the_groups():
    text = (ROOT / "mkdocs.yml").read_text(encoding="utf-8")
    nav = text.split("nav:", 1)[1].split("plugins:", 1)[0]
    assert tuple(GROUP.findall(nav)) == GROUPS
    items = ITEM.findall(nav)
    assert len(items) == len(TITLES)
    by_slug = {slug_for(stem): (stem, title) for stem, title in TITLES.items()}
    for label, slug in items:
        stem, title = by_slug[slug]
        assert label == title, stem


def test_callout_keeps_its_label_and_a_blank_line_inside():
    text = "> [!NOTE]\n> One\n>\n> Two\n\nAfter\n"
    out = convert_page("Install", text, {"Install": text})
    assert '!!! note "Note"' in out
    assert "\n    One\n\n    Two\n" in out
    assert out.endswith("\nAfter\n") or "\nAfter\n" in out
    assert out.startswith("# Install\n")


def test_code_external_and_anchor_links_stay():
    pages = {"Install": "x", "Home": "x"}
    text = (
        "See `[Install](Install)` and [Install](Install#the-three-build-tiers) and "
        "[site](https://byvfx.github.io/floki/) and [here](#local).\n"
    )
    out = convert_page("Home", text, pages)
    assert "`[Install](Install)`" in out
    assert "](install.md#the-three-build-tiers)" in out
    assert "https://byvfx.github.io/floki/" in out
    assert "](#local)" in out
    assert "template: home.html" in out


def test_home_strips_the_wiki_note_and_uses_the_home_template():
    pages = _pages()
    out = convert_page("Home", pages["Home"], pages)
    assert "template: home.html" in out
    assert "\nhide:\n  - toc\n" in out
    assert "wiki-only" not in out
    assert "This wiki mirrors" not in out
    assert "byvfx.github.io/floki" not in out
    assert '!!! tip "Tip"' in out
    assert '<table markdown="block">' in out
    body = out.split("---", 2)[-1]
    assert not body.lstrip().startswith("# ")


def test_wiki_only_block_is_dropped_and_a_mention_in_code_stays():
    text = (
        "See `<!-- wiki-only -->` in the source.\n\n"
        "<!-- wiki-only -->\n"
        "> [!NOTE]\n"
        "> Only on the wiki.\n"
        "<!-- /wiki-only -->\n\n"
        "After [Install](Install)\n"
    )
    out = convert_page("Install", text, {"Install": text})
    assert "Only on the wiki" not in out
    assert "> [!" not in out
    assert "`<!-- wiki-only -->`" in out
    assert "\nAfter " in out
    assert "](install.md)" in out
    assert out.startswith("# Install\n")


def test_unclosed_wiki_only_block_fails():
    pages = {"Install": ""}
    with pytest.raises(RuntimeError, match="wiki-only"):
        convert_page("Install", "<!-- wiki-only -->\n> [!NOTE]\n> Hi\n", pages)
    with pytest.raises(RuntimeError, match="wiki-only"):
        convert_page("Install", "Before\n<!-- /wiki-only -->\n", pages)


def test_wiki_only_markers_inside_a_fence_stay():
    text = "```\n<!-- wiki-only -->\nkeep\n<!-- /wiki-only -->\n```\n\nAfter\n"
    out = convert_page("Install", text, {"Install": text})
    assert "<!-- wiki-only -->" in out
    assert "keep" in out
    assert "\nAfter\n" in out


def test_picture_follows_the_theme_toggle():
    text = (
        '<picture><source media="(prefers-color-scheme: dark)" srcset="dark.png">'
        '<img src="light.png" alt="Floki"></picture>\n'
    )
    out = convert_page("Install", text, {"Install": text})
    assert 'class="nw-img-dark"' in out
    assert 'class="nw-img-light"' in out
    assert "dark.png" in out
    assert "light.png" in out
    assert "<picture>" not in out


def test_every_wiki_page_converts_without_losing_a_link():
    pages = _pages()
    slugs = {slug_for(stem) for stem in pages if not stem.startswith("_")}
    for stem, text in pages.items():
        if stem.startswith("_"):
            continue
        out = convert_page(stem, text, pages)
        prose = _prose(out)
        assert "> [!" not in prose, stem
        for line in ADMONITION.findall(out):
            kind, label = line
            assert kind in {"note", "tip", "important", "warning", "caution"}, (stem, line)
            assert label, stem
        for target in LINK.findall(prose):
            if re.match(r"(https?|mailto):", target) or target.startswith("#"):
                continue
            assert re.fullmatch(r"[a-z0-9-]+\.md(#[\w-]*)?", target), f"{stem}: {target}"
            assert target.split("#", 1)[0][:-3] in slugs, f"{stem}: {target}"
        if stem != "Home":
            assert out.startswith(f"# {TITLES[stem]}\n"), stem


def test_generate_writes_pages_and_leaves_the_wiki_alone(tmp_path: Path):
    wiki = tmp_path / "wiki"
    docs = tmp_path / "docs"
    wiki.mkdir()
    docs.mkdir()
    (wiki / "Home.md").write_text("Hello [Install](Install)\n\n> [!TIP]\n> Go.\n", encoding="utf-8")
    (wiki / "Install.md").write_text("Do the install.\n", encoding="utf-8")
    (wiki / "_Sidebar.md").write_text("- [Home](Home)\n", encoding="utf-8")
    before = {path.name: path.read_bytes() for path in wiki.iterdir()}
    assert generate(docs, wiki) == 2
    assert {path.name: path.read_bytes() for path in wiki.iterdir()} == before
    assert (docs / "index.md").is_file()
    assert (docs / "install.md").is_file()
    assert not (docs / "_sidebar.md").exists()
    assert "install.md" in (docs / "index.md").read_text(encoding="utf-8")
    # A second build replaces generated pages instead of keeping a stale one.
    (wiki / "Install.md").unlink()
    assert generate(docs, wiki) == 1
    assert not (docs / "install.md").exists()
