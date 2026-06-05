#!/usr/bin/env python3
"""English-only gate: fail if German text leaks into the repo.

The repository content is English-only (conversation may be German, the repo is
not). This check scans tracked text files for German signals and fails with an
agent-actionable report so a change can be fixed and re-submitted.

Detection (kept deliberately low-false-positive):
  * the eszett (ß) — German-specific;
  * any word containing an umlaut (ä ö ü Ä Ö Ü);
  * a curated set of unambiguous German words (no common English meaning).

Legitimate non-English text is allowlisted three ways:
  * tools/lang_allowlist.txt — "glob:<pattern>" exempts whole files; any other
    non-comment line is an allowed substring (a line containing it is skipped);
  * an inline `lang-allow` marker comment on a single source line.

Exit code 1 on any unallowed occurrence, 0 otherwise.
"""
from __future__ import annotations

import fnmatch
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ALLOWLIST = ROOT / "tools" / "lang_allowlist.txt"

TEXT_EXT = {
    ".rs", ".md", ".py", ".toml", ".yml", ".yaml", ".json", ".html", ".css",
    ".js", ".ts", ".txt", ".cfg", ".service", ".sh",
}

# Unambiguous German words (no common English meaning). Whole-word, case-insensitive.
# Deliberately excludes German words that collide with English (die, war, also, hat,
# man, will, fast, in, an, so, ...) to avoid false positives.
GERMAN_WORDS = {
    # function words
    "und", "oder", "nicht", "für", "sind", "wird", "werden", "auch", "sehr",
    "diese", "dieser", "dieses", "kann", "muss", "soll", "beim", "vom", "zum",
    "zur", "deine", "deinen", "ihre", "wenn", "wurde", "wurden", "keine", "noch",
    "schon", "durch", "gegen", "ohne", "über", "unter", "zwischen", "sowie",
    # UI / domain words (seen in this project + common desktop-app German)
    "fehler", "gedrosselt", "synchronisiert", "pausiert", "fortsetzen", "öffnen",
    "schließen", "löschen", "speichern", "abbrechen", "einstellungen", "datei",
    "hinzufügen", "aktualisieren", "anmelden", "abmelden", "wiederherstellen",
    "suchen", "konto", "leitung", "benachrichtigung", "verbinden", "verbindung",
}

WORD_RE = re.compile(r"[A-Za-zÄÖÜäöüß]+")
UMLAUT_RE = re.compile(r"[ÄÖÜäöüß]")


def load_allowlist() -> tuple[list[str], list[str]]:
    globs: list[str] = []
    subs: list[str] = []
    if ALLOWLIST.exists():
        for raw in ALLOWLIST.read_text(encoding="utf-8").splitlines():
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            if line.startswith("glob:"):
                globs.append(line[len("glob:"):].strip())
            else:
                subs.append(line)
    return globs, subs


def tracked_text_files() -> list[str]:
    out = subprocess.run(
        ["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout
    return [rel for rel in out.splitlines() if Path(rel).suffix.lower() in TEXT_EXT]


def is_german(tok: str) -> bool:
    return bool(UMLAUT_RE.search(tok)) or tok.lower() in GERMAN_WORDS


def main() -> int:
    globs, subs = load_allowlist()
    hits: list[tuple[str, int, int, str, str]] = []
    for rel in tracked_text_files():
        if any(fnmatch.fnmatch(rel, g) for g in globs):
            continue
        try:
            text = (ROOT / rel).read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        lines = text.splitlines()
        for lineno, line in enumerate(lines, start=1):
            prev = lines[lineno - 2] if lineno >= 2 else ""
            # `lang-allow` marker applies to its own line or the line right below it.
            if "lang-allow" in line or "lang-allow" in prev:
                continue
            if any(sub in line for sub in subs):
                continue
            for m in WORD_RE.finditer(line):
                if is_german(m.group(0)):
                    hits.append((rel, lineno, m.start() + 1, m.group(0), line.strip()[:120]))
                    break

    if hits:
        print(f"language-check: FAIL — {len(hits)} German text occurrence(s); the repo is English-only.\n")
        for rel, lineno, col, tok, ctx in hits:
            print(f"  {rel}:{lineno}:{col}: German token '{tok}'")
            print(f"      | {ctx}")
        print("\nResolve each occurrence by either:")
        print("  - translating the text to English (repo content is English-only), or")
        print("  - if it is legitimate (a test fixture, a locale file, or an umlaut-coverage")
        print("    note), add an inline `lang-allow` marker comment on that line (or the line")
        print("    directly above it), or add the file glob / substring to tools/lang_allowlist.txt.")
        return 1

    print("language-check: OK — no German text in tracked English-only files.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
