"""docs/wiki/ is pushed to the GitHub wiki (.github/workflows/wiki.yml): every page link must
resolve, and the sidebar must list every page."""
import re

from conftest import ROOT

WIKI = ROOT / "docs" / "wiki"
LINK = re.compile(r"\]\(([^)\s]+)\)")
HREF = re.compile(r'href="([^"]+)"')
DOCS = "https://byvfx.github.io/floki/"


def _pages():
    return {p.stem: p.read_text(encoding="utf-8") for p in WIKI.glob("*.md")}


def _no_blocks(text):
    return re.sub(r"^```.*?^```", "", text, flags=re.DOTALL | re.MULTILINE)


def _prose(text):
    """The text without code blocks and inline code, where `](` is not a link."""
    return re.sub(r"`[^`\n]*`", "", _no_blocks(text))


def _anchors(text):
    """GitHub's heading ids: lowercase, punctuation dropped, spaces to hyphens."""
    heads = re.findall(r"^#+ (.+)$", _no_blocks(text), flags=re.MULTILINE)
    return {re.sub(r"[^\w\- ]", "", h.strip().lower()).replace(" ", "-") for h in heads}


def test_page_links_resolve():
    pages = _pages()
    assert "Home" in pages
    bad = []
    for name, text in pages.items():
        for target in LINK.findall(_prose(text)) + HREF.findall(text):
            if re.match(r"(https?|mailto):", target):
                continue
            page, _, anchor = target.partition("#")
            page = page or name
            if page.endswith(".md"):
                page = page[:-3]
            if page not in pages:
                bad.append(f"{name}: {target} (no page {page})")
            elif anchor and anchor not in _anchors(pages[page]):
                bad.append(f"{name}: {target} (no heading #{anchor} in {page})")
    assert not bad, "\n".join(bad)


def test_sidebar_lists_every_page():
    pages = _pages()
    listed = set()
    for target in LINK.findall(pages["_Sidebar"]):
        page = target.partition("#")[0]
        if page.endswith(".md"):
            page = page[:-3]
        listed.add(page)
    missing = sorted(p for p in pages if not p.startswith("_") and p not in listed)
    assert not missing, "not in docs/wiki/_Sidebar.md: " + ", ".join(missing)


def test_anchor_slugs():
    text = "## OpenColorIO (#24)\n## The walkthrough: `cargo ocio-run`\n```bash\n# a comment\n```\n"
    assert _anchors(text) == {"opencolorio-24", "the-walkthrough-cargo-ocio-run"}


def test_wiki_points_readers_at_the_docs_site():
    """Home opens with a wiki-only note, and the sidebar sends readers to the docs site."""
    pages = _pages()
    home = pages["Home"]
    assert home.startswith("<!-- wiki-only -->")
    note = home.index("<!-- wiki-only -->")
    close = home.index("<!-- /wiki-only -->")
    intro = home.index("**Floki**")
    assert note < close < intro
    block = home[note:close]
    assert "> [!NOTE]" in block
    assert DOCS in block
    assert "This wiki mirrors" in block
    assert DOCS in pages["_Sidebar"]
