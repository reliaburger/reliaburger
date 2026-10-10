"""Checks that keep the book's table of contents and its links honest.

The chapters are listed in two places: the chapter table in CLAUDE.md, which
tells contributors where material goes, and the Quarto book profile, which
decides what the PDF contains. A chapter missing from either is invisible to
someone. Relative links between book files break silently when a section moves
to another chapter, so every one is resolved against the files and headings on
disk.
"""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
BOOK = ROOT / "docs" / "book"
QUARTO_BOOK = ROOT / "docs" / "_quarto" / "_quarto-book.yml"
CLAUDE = ROOT / "CLAUDE.md"

FENCE = re.compile(r"^(```|~~~)")
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")
LINK = re.compile(r"(?<!!)\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
EXPLICIT_ID = re.compile(r"\{#([^}\s]+)[^}]*\}\s*$")


def mapped_chapters():
    """Chapter files named in the CLAUDE.md chapter table."""
    text = CLAUDE.read_text(encoding="utf-8")
    return re.findall(r"^\|[^|]*\|\s*`([^`]+\.md)`\s*\|", text, re.MULTILINE)


def built_chapters():
    """Chapter files under book/ that the Quarto book profile renders."""
    text = QUARTO_BOOK.read_text(encoding="utf-8")
    return re.findall(r"^\s*-\s*book/(\S+\.md)\s*$", text, re.MULTILINE)


def slug(heading):
    """The anchor GitHub and Quarto both give a heading."""
    explicit = EXPLICIT_ID.search(heading)
    if explicit:
        return explicit.group(1)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", heading)
    text = text.lower().replace("`", "")
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", "-")


def anchors(path):
    """Every heading anchor in a Markdown file, skipping fenced code."""
    found = set()
    seen = {}
    in_fence = False
    for line in path.read_text(encoding="utf-8").splitlines():
        if FENCE.match(line):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        match = HEADING.match(line)
        if not match:
            continue
        base = slug(match.group(2))
        count = seen.get(base, 0)
        seen[base] = count + 1
        found.add(base if count == 0 else f"{base}-{count}")
    return found


def links(path):
    """Relative links in a Markdown file, outside fenced code."""
    in_fence = False
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if FENCE.match(line):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        for target in LINK.findall(line):
            if re.match(r"^[a-z][a-z0-9+.-]*:", target):
                continue
            yield number, target


class BookTests(unittest.TestCase):
    def test_every_book_chapter_is_mapped_and_built(self):
        on_disk = {path.name for path in BOOK.glob("[0-9]*.md")}
        mapped = set(mapped_chapters())
        built = set(built_chapters())
        self.assertTrue(mapped, "CLAUDE.md has no chapter table")
        self.assertEqual(sorted(mapped - on_disk), [], "mapped chapters with no file")
        self.assertEqual(sorted(on_disk - mapped), [], "chapters missing from CLAUDE.md")
        self.assertEqual(sorted(on_disk - built), [], "chapters missing from the book build")
        self.assertEqual(sorted(built - on_disk), [], "book build lists missing chapters")
        self.assertEqual(built_chapters()[-1], "16-appendix-rust.md", "the appendix stays last")

    def test_book_links_resolve(self):
        broken = []
        cache = {}
        for page in sorted(BOOK.glob("*.md")):
            for number, target in links(page):
                path_part, _, fragment = target.partition("#")
                destination = (page.parent / path_part).resolve() if path_part else page
                where = f"{page.relative_to(ROOT)}:{number}: {target}"
                if not destination.exists():
                    broken.append(f"{where} (no such file)")
                    continue
                if not fragment or destination.suffix != ".md":
                    continue
                if destination not in cache:
                    cache[destination] = anchors(destination)
                if fragment not in cache[destination]:
                    broken.append(f"{where} (no such heading)")
        self.assertEqual(broken, [])

    def test_slug_matches_github_anchors(self):
        self.assertEqual(slug("Definitions, runs and durable trigger identities"),
                         "definitions-runs-and-durable-trigger-identities")
        self.assertEqual(slug("Bloom filters help equality, not `LIKE`"),
                         "bloom-filters-help-equality-not-like")
        self.assertEqual(slug("Two-phase GC, or: the check-then-act bug"),
                         "two-phase-gc-or-the-check-then-act-bug")


if __name__ == "__main__":
    unittest.main()
