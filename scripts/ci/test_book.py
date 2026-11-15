"""Checks that keep the book's table of contents and its links honest.

The chapters are listed in two places: the chapter table in CLAUDE.md, which
tells contributors where material goes, and the Quarto book profile, which
decides what the PDF contains. A chapter missing from either is invisible to
someone. Relative links between book files break silently when a section moves
to another chapter, so every one is resolved against the files and headings on
disk.

The book also promises that a reader can follow along in the source, so every
repository path it cites must exist, and every listing captioned "(from
`src/...`)" must show a type or function that file still defines. Test counts
go stale with every commit, so the book doesn't quote them.
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
REPO_PATH = re.compile(
    r"(?<![\w/.-])((?:src|tests|scripts|ebpf|benches|examples)/[A-Za-z0-9_./*-]*[A-Za-z0-9_*])"
)
CAPTION = re.compile(r"^\(from `([^`]+)`")
DEFINITION = re.compile(r"\b(?:fn|struct|trait|enum|type)\s+([A-Za-z_]\w*)")
TEST_COUNT = re.compile(
    r"\b\d[\d,]*\s+(?:[a-z-]+\s+)?tests\b|bringing the total to",
    re.IGNORECASE,
)

# Paths the book names on purpose although they're gone: each passage tells
# the story of deleting them.
DELETED_ON_PURPOSE = {
    ("08-breaking-things-on-purpose.md", "tests/chaos_smoker.rs"),
    ("10-locking-it-down.md", "tests/auth_memory.rs"),
}


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


def chapters():
    """The book's chapter files, without the style guide."""
    return sorted(BOOK.glob("[0-9]*.md"))


def prose(path):
    """(line number, line) for each line outside fenced code."""
    in_fence = False
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if FENCE.match(line):
            in_fence = not in_fence
            continue
        if not in_fence:
            yield number, line


def cited_paths(path):
    """(line number, repository path) for each path the prose cites."""
    for number, line in prose(path):
        for cited in REPO_PATH.findall(line):
            yield number, cited.rstrip(".")


def path_exists(cited):
    if "*" in cited:
        return any(ROOT.glob(cited))
    return (ROOT / cited).exists()


def captioned_listings(path):
    """(line number, cited path, listing text) for each "(from `...`)" caption.

    A caption sits on its own line just after the listing it describes.
    """
    lines = path.read_text(encoding="utf-8").splitlines()
    for index, line in enumerate(lines):
        match = CAPTION.match(line)
        if not match:
            continue
        end = index - 1
        while end >= 0 and not lines[end].strip():
            end -= 1
        if end < 0 or not FENCE.match(lines[end]):
            continue
        start = end - 1
        while start >= 0 and not FENCE.match(lines[start]):
            start -= 1
        yield index + 1, match.group(1), "\n".join(lines[start + 1:end])


def source_text(cited):
    """The text of a cited file, or of every Rust file under a cited directory."""
    target = ROOT / cited
    if target.is_dir():
        return "\n".join(p.read_text(encoding="utf-8") for p in sorted(target.rglob("*.rs")))
    return target.read_text(encoding="utf-8")


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

    def test_book_cites_only_files_that_exist(self):
        missing = [
            f"{page.relative_to(ROOT)}:{number}: {cited}"
            for page in chapters()
            for number, cited in cited_paths(page)
            if not path_exists(cited) and (page.name, cited) not in DELETED_ON_PURPOSE
        ]
        self.assertEqual(missing, [])

    def test_deleted_on_purpose_paths_are_still_cited_and_still_gone(self):
        for chapter, cited in sorted(DELETED_ON_PURPOSE):
            self.assertFalse(path_exists(cited), f"{cited} exists again; drop it from the list")
            named = {path for _, path in cited_paths(BOOK / chapter)}
            self.assertIn(cited, named, f"{chapter} no longer names {cited}")

    def test_book_code_identifiers_exist(self):
        stale = []
        for page in chapters():
            for number, cited, listing in captioned_listings(page):
                where = f"{page.relative_to(ROOT)}:{number}: {cited}"
                if not (ROOT / cited).exists():
                    stale.append(f"{where} (no such file)")
                    continue
                definition = DEFINITION.search(listing)
                if definition and not re.search(
                    rf"\b{re.escape(definition.group(1))}\b", source_text(cited)
                ):
                    stale.append(f"{where} (no {definition.group(1)})")
        self.assertEqual(stale, [])

    def test_book_quotes_no_test_counts(self):
        counts = [
            f"{page.relative_to(ROOT)}:{number}: {match.group(0)}"
            for page in chapters()
            for number, line in prose(page)
            for match in TEST_COUNT.finditer(line)
        ]
        self.assertEqual(counts, [])

    def test_test_count_pattern_spares_named_tests(self):
        self.assertIsNone(TEST_COUNT.search("Two tests pin it down."))
        self.assertIsNotNone(TEST_COUNT.search("Phase 3 adds 114 tests, bringing the total to 702."))
        self.assertIsNone(TEST_COUNT.search("Three tests in `tests/runc_roles.rs` cover it."))
        self.assertIsNotNone(TEST_COUNT.search("The 104 tests in `src/pickle/` cover:"))

    def test_slug_matches_github_anchors(self):
        self.assertEqual(slug("Definitions, runs and durable trigger identities"),
                         "definitions-runs-and-durable-trigger-identities")
        self.assertEqual(slug("Bloom filters help equality, not `LIKE`"),
                         "bloom-filters-help-equality-not-like")
        self.assertEqual(slug("Two-phase GC, or: the check-then-act bug"),
                         "two-phase-gc-or-the-check-then-act-bug")


if __name__ == "__main__":
    unittest.main()
