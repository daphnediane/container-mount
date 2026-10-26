---
trigger: model_decision
description: English and spelling conventions — American English, real words over abbreviations, cSpell dictionary policy
---

# English and spelling

Applies to code, comments, docs, shell scripts, and commit messages.

<!-- cspell:ignore defence nums toks subdirs rederives CLOEXEC -->

- **American English** (`defense` not `defence`).
- **Prefer the clearer, simpler phrasing** — plain words over formal or
  clever ones (`use` not `utilize`, `start` not `commence`), direct
  sentences over nested clauses. Write for the reader skimming at
  2 a.m., not for sounding thorough.
- **Prefer real words over abbreviations** anywhere a reader has to
  guess (`numbers` not `nums`, `tokens` not `toks`, `subdirectories`
  not `subdirs`). Fix the word rather than dictionary-adding it when
  the flagged term is just sloppy.
- **Match external APIs verbatim** — `O_CLOEXEC`, `localtime_r`,
  `st_birthtim` keep their upstream spelling even where it's
  ungrammatical.
- **Domain coinages are fine when productive and unambiguous**
  (`unhex`, `reflinked`, `writability`). If one reads awkwardly,
  prefer plain English (`recomputes` not `rederives`).
- **Name one-way tools with direction-clear verbs** — a script that
  only pulls upstream is `refresh-*`, not `update-*` (which reads as
  pushing).

## cSpell

- **Fix real misspellings; don't blanket-add flagged words.** Decide
  per word: fix, dictionary entry, or local ignore.
- **Add full symbols, not chunks** — `O_CLOEXEC`, not `CLOEXEC`. Chunk
  entries hide the same misspelling elsewhere. A chunk earns its own
  entry only when it stands alone as a word in our code (`nlink`,
  `rdev` as protocol field names).
- **No uppercase variants** of words already listed in lowercase.
- **Opaque identifiers** (team IDs, hashes, `ls -l` mode strings) get
  a local `// cspell:ignore` / `# cspell:ignore` /
  `<!-- cspell:ignore -->` near the occurrence, not a dictionary entry.
- **Misspellings cited as examples** (in rule docs like this one) get
  a local `<!-- cspell:ignore -->` at the top of that doc — never a
  dictionary entry, or the misspelling would pass everywhere. (Not a
  path-scoped `overrides` block in `cSpell.json`: it doesn't apply
  correctly in multi-folder workspaces.)
- **Dictionary layout** under `.vscode/dictionaries/`: `words.txt` is
  the general list — listed first in `dictionaries` and the
  `addWords` target so IDE quick-adds land there; `packages.txt` is
  crate/tool names only.
- **Generated files** (`THIRD-PARTY-NOTICES.yaml`) are `ignorePaths`,
  not dictionary fodder. `Cargo.lock` stays checked — an unknown
  dependency name surfacing as a spelling flag is a cheap typosquat
  tripwire.
