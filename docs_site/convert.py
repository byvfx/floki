"""Turn a GitHub wiki page into a MkDocs page.

docs/wiki/ stays the source the GitHub wiki renders. This module rewrites links and
callouts only in the copy MkDocs builds. A ``<!-- wiki-only -->`` ... ``<!-- /wiki-only -->``
block is kept on the wiki and dropped here (HTML comments do not render on GitHub).
"""

from __future__ import annotations

import html
import re
from pathlib import Path

# Sidebar labels. Home is the hero, not an h1 in the article.
TITLES: dict[str, str] = {
    "Batch-tools": "Batch tools",
    "Color-management": "Color management",
    "Comparison": "Comparison",
    "Concurrency-contract": "Concurrency contract",
    "Cross-player-benchmark": "Cross-player benchmark",
    "Developing": "Developing",
    "Home": "Home",
    "Inspection": "Inspection",
    "Install": "Install",
    "Install-Linux": "Linux",
    "Install-Windows": "Windows",
    "Install-macOS": "macOS",
    "Keyboard-shortcuts": "Keyboard shortcuts",
    "Layer-model": "Layer model",
    "Layers-and-compositing": "Layers and compositing",
    "Memory-contract": "Memory contract",
    "Architecture": "Architecture",
    "Patched-exr-fork": "Patched exr fork",
    "Perf-roadmap": "Perf roadmap",
    "Playback": "Playback",
    "Releasing": "Releasing",
    "Review-player-roadmap": "Review player roadmap",
    "Sequence-playback": "Sequence playback",
    "Testing": "Testing",
    "Troubleshooting": "Troubleshooting",
}

_FENCE = re.compile(r"^[ \t]*```.*?^[ \t]*```", re.M | re.S)
_INLINE = re.compile(r"`[^`\n]*`")
_BANNER = re.compile(r'<p align="center">\s*<picture>.*?</picture>\s*</p>\s*', re.S | re.I)
_PICTURE = re.compile(r"<picture>(.*?)</picture>", re.S | re.I)
_LINK = re.compile(r"(?<!!)\[([^\]]+)\]\(([^)\s]+)\)")
_ALERT = re.compile(
    r"^> \[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\][^\n]*\n((?:>(?:[ \t][^\n]*)?\n?)*)",
    re.M,
)
_H1 = re.compile(r"^# ", re.M)
# One-line HTML comments. GitHub shows the block between them; the site build drops it.
_WIKI_ONLY_BLOCK = re.compile(
    r"(?m)^[ \t]*<!--\s*wiki-only\s*-->.*?<!--\s*/wiki-only\s*-->[ \t]*\n?",
    re.S,
)
_WIKI_ONLY_MARK = re.compile(r"<!--\s*/?\s*wiki-only\s*-->")
_ALERT_LABEL = {
    "NOTE": "Note",
    "TIP": "Tip",
    "IMPORTANT": "Important",
    "WARNING": "Warning",
    "CAUTION": "Caution",
}


def github_slugify(value, separator, unicode=False):
    """GitHub wiki heading ids: punctuation dropped, spaces become hyphens.

    Python-Markdown passes the heading's plain text (tags already stripped, entities
    unescaped). Backticks and other punctuation drop out either way, so a slug from
    the raw heading line matches one from that plain text.
    """
    del unicode
    text = html.unescape(str(value)).strip().lower()
    text = text.replace("\xa0", " ")
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", separator)


def slug_for(stem: str) -> str:
    """MkDocs filename stem. Home is the site index."""
    return "index" if stem == "Home" else stem.lower()


def convert_page(stem: str, text: str, pages: dict[str, str]) -> str:
    """One wiki page, ready to write as ``<slug>.md``."""
    text = text.replace("\r\n", "\n")
    if stem == "Home":
        text = _BANNER.sub("", text, count=1)
    text, slots = _mask(text)
    text = _drop_wiki_only(text, stem)
    text = _PICTURE.sub(_picture_repl, text)
    text = _LINK.sub(lambda m: _link_repl(m, pages), text)
    text = _ALERT.sub(_alert_repl, text)
    if stem == "Home":
        # md_in_html only parses a <td> when the <table> and <tr> are markdown blocks too.
        text = re.sub(r"<(table|tr|td)\b([^>]*)>", _md_block_repl, text)
    text = _unmask(text, slots)
    return _title(stem, text)


def generate(docs_dir: Path, wiki_dir: Path) -> int:
    """Write ``docs_dir/*.md`` from ``wiki_dir``. Returns how many pages were written.

    Deletes previously generated markdown in ``docs_dir`` (not subfolders). Does not
    write inside ``wiki_dir``. ``_``-prefixed wiki files (sidebar, footer) are skipped.
    """
    docs_dir = Path(docs_dir)
    pages = {path.stem: path.read_text(encoding="utf-8") for path in sorted(wiki_dir.glob("*.md"))}
    for old in docs_dir.glob("*.md"):
        old.unlink()
    written = 0
    seen: dict[str, str] = {}
    for stem, text in pages.items():
        if stem.startswith("_"):
            continue
        slug = slug_for(stem)
        if slug in seen:
            raise RuntimeError(f"docs slug {slug!r} is used by {seen[slug]!r} and {stem!r}")
        seen[slug] = stem
        out = convert_page(stem, text, pages)
        if not out.endswith("\n"):
            out += "\n"
        (docs_dir / f"{slug}.md").write_text(out, encoding="utf-8")
        written += 1
    return written


def _title(stem: str, text: str) -> str:
    body = text.lstrip("\n")
    if stem == "Home":
        # The cards are the index. A side table of contents would narrow the column
        # the home mockup draws full width.
        return "---\ntitle: Home\ntemplate: home.html\nhide:\n  - toc\n---\n\n" + body
    if _H1.search(_mask(body)[0]):
        return body
    return f"# {TITLES[stem]}\n\n{body}"


def _drop_wiki_only(text: str, stem: str) -> str:
    """Drop wiki-only blocks. Inline code and fences are already masked, so a mention stays."""
    dropped = _WIKI_ONLY_BLOCK.sub("", text)
    if _WIKI_ONLY_MARK.search(dropped):
        raise RuntimeError(f"{stem}: <!-- wiki-only --> markers do not pair")
    return dropped


def _mask(text: str) -> tuple[str, list[str]]:
    slots: list[str] = []

    def stash(match: re.Match[str]) -> str:
        slots.append(match.group(0))
        return f"@@CODE{len(slots) - 1}@@"

    text = _FENCE.sub(stash, text)
    text = _INLINE.sub(stash, text)
    return text, slots


def _unmask(text: str, slots: list[str]) -> str:
    for i in range(len(slots) - 1, -1, -1):
        text = text.replace(f"@@CODE{i}@@", slots[i])
    return text


def _picture_repl(match: re.Match[str]) -> str:
    inner = match.group(1)
    source = re.search(r'<source\b[^>]*\bsrcset="([^"]+)"', inner)
    img = re.search(r"<img\b([^>]*)>", inner)
    if source is None or img is None:
        return match.group(0)
    src = re.search(r'\bsrc="([^"]+)"', img.group(1))
    if src is None:
        return match.group(0)
    rest = re.sub(r'\s*\bsrc="[^"]*"', "", img.group(1)).strip()
    attrs = f" {rest}" if rest else ""
    return (
        '<span class="nw-figure">'
        f'<img class="nw-img-dark" src="{source.group(1)}"{attrs}>'
        f'<img class="nw-img-light" src="{src.group(1)}"{attrs}>'
        "</span>"
    )


def _convert_target(target: str, pages: dict[str, str]) -> str | None:
    if re.match(r"(https?:|mailto:)", target) or target.startswith("#"):
        return None
    path, sep, anchor = target.partition("#")
    if path.endswith(".md"):
        path = path[:-3]
    if path not in pages or path.startswith("_"):
        return None
    dest = f"{slug_for(path)}.md"
    if sep:
        dest = f"{dest}#{anchor}"
    return dest


def _link_repl(match: re.Match[str], pages: dict[str, str]) -> str:
    dest = _convert_target(match.group(2), pages)
    if dest is None:
        return match.group(0)
    return f"[{match.group(1)}]({dest})"


def _alert_repl(match: re.Match[str]) -> str:
    kind = match.group(1)
    lines = [f'!!! {kind.lower()} "{_ALERT_LABEL[kind]}"', ""]
    for line in _alert_lines(match.group(2)):
        lines.append(f"    {line}" if line else "")
    return "\n".join(lines) + "\n"


def _alert_lines(body: str) -> list[str]:
    lines = []
    for line in body.split("\n"):
        if line.startswith("> "):
            lines.append(line[2:])
        elif line.startswith(">"):
            lines.append(line[1:])
        else:
            lines.append(line)
    while lines and lines[-1] == "":
        lines.pop()
    return lines


def _md_block_repl(match: re.Match[str]) -> str:
    tag, attrs = match.group(1), match.group(2)
    if "markdown=" in attrs:
        return match.group(0)
    return f'<{tag}{attrs} markdown="block">'
