# 🦊 ferox v0.3.1

> Released: 2026-10-24

---

## ✨ What's New

### ▶ Run Selection

Select part of the editor and press **Ctrl+Enter** / **F5** (or click **Run** / **Explain**) — only the selected SQL runs. While something is selected, the Run button reads **▶ Run Selection**, so it's always clear what will execute. With no selection, the whole editor runs exactly as before.

### ✦ AI: Explain, Optimize & Fix Error

The AI button has a new **✦ ▾** menu next to it (works on the selection, if any):

- **Explain query** — a plain-language walkthrough of what the query returns, how tables are joined and filtered, and pitfalls worth knowing (NULL handling, join fan-out, missing `LIMIT`, unindexed filters)
- **Optimize query** — a rewrite that returns the same result faster, using your live schema **and its indexes**; if an index would help more than a rewrite, the suggested `CREATE INDEX` is in the explanation
- **Fix error with AI** — enabled after a query fails (also offered as a button in the Messages tab); sends the query plus the Postgres error and returns a minimal fix with the cause explained

AI-generated SQL is **never executed automatically**. It opens in a review window with **Replace in editor** / **Copy SQL**, and if the suggestion contains a write operation the original didn't have (`DELETE`, `DROP`, `UPDATE`, …) a red warning names it. Replacing the editor contents saves your original query to history first — press ↑ to get it back. Works with every configured provider (Claude, OpenAI, Groq, Ollama, custom).

### 🛠 Table Structure Editor

Right-click a table → **Edit Table** is now a full structure editor:

- **Columns** — rename, change type (with an automatic `USING col::newtype` cast), set/drop `NOT NULL`, set/drop `DEFAULT`, drop, add
- **Indexes** — drop existing ones (PK/UNIQUE-backed indexes correctly become `DROP CONSTRAINT`), add new ones with column picker, `UNIQUE`, and method (`btree`, `hash`, `gin`, `gist`, `brin`)
- **Rename the table**
- **Live DDL preview** — the exact SQL is always shown, syntax-highlighted, before anything runs; risky changes (dropped columns, type rewrites, `SET NOT NULL`, dropped indexes) are listed in red
- **All-or-nothing** — the whole change set runs as a single transaction, so a failing step leaves the table untouched
- **Open in Editor** puts the DDL into a new query tab instead of running it
- If the database rejects the change, the dialog stays open with the error — your edits aren't lost

### 💾 Schema Dump (no `pg_dump` needed)

Right-click a schema → **Dump schema (structure + data)…** or **(structure only)…** writes a plain `.sql` file you can run anywhere — including through ferox's own SSH tunnel, with no local `pg_dump` / version matching required.

- Covers enum & domain types, sequences (with current values), functions & procedures, tables (identity, generated & collated columns, partitions), CHECK / PK / UNIQUE / EXCLUDE / foreign keys, indexes, views, materialized views, and triggers — in restore-safe order (keys and triggers after the data)
- One consistent snapshot (`REPEATABLE READ READ ONLY`); rows stream through a server-side cursor straight to disk, so table size doesn't affect memory
- Values are written exactly as the server renders them — no type round-trip
- A failed or cancelled dump never leaves a half-written file behind
- Not included: owners, privileges, RLS policies, comments (like `pg_dump --no-owner --no-privileges`). Requires PostgreSQL 12+

### 📥 Export as INSERT Statements

New **INSERT** button next to CSV / JSON, plus **Export Selected as INSERT…** in the row right-click menu. Generates batched `INSERT INTO … VALUES` statements (100 rows each). The target table is taken from the browsed table, or inferred from a single-table query.

### 🦫 Import Connections from DBeaver

**Connection → Import from DBeaver…** finds your DBeaver workspace automatically (Windows, macOS, Linux, Flatpak, Snap) — or pick a `data-sources.json` by hand.

- Imports PostgreSQL connections with **passwords**, SSH tunnel, SSL mode, folders (→ groups) and DBeaver's prod/test connection type (→ color tag)
- Preview list with checkboxes; connections you already have are detected and unchecked
- Imported passwords go straight into the OS keychain

### ⬆ Update Check

ferox now tells you when a new release is out. One request to the GitHub releases API at startup, on a background thread (the UI never waits on the network). A banner offers **Release notes / download**, **Skip this version**, or dismiss. Nothing is downloaded or installed automatically.

- **Settings → Check for updates on startup** turns it off
- **Settings → Check for updates now** runs a check on demand

---

## 🐛 Fixes

| # | Description |
|---|---|
| 1 | **Ctrl+Enter ran the query twice** when the editor had focus — once from the global shortcut and once from the editor itself. A DML statement could insert/update rows twice. Ctrl+Enter / F5 now go through a single path, which also means **safe mode always applies** to them |
| 2 | All query tabs shared one editor state, so a cursor/selection from one tab leaked into the next — each tab now has its own editor state (cursor, selection, undo) |
| 3 | Edit Table: a failed `ALTER` closed the dialog and discarded every edit — the dialog now stays open and shows the error |
| 4 | Edit Table: sidebar column/index details stayed stale after a structure change — they're reloaded once the DDL succeeds |
| 5 | F5 on a multi-statement script showed only the last result; it now opens one result tab per statement, same as the Run button |

---

## ⚡ Performance

- Schema dump streams rows through a cursor in 1,000-row batches — memory use doesn't scale with table size, and the 50k-row result cap doesn't apply
- The startup update check's HTTP/TLS stack is released again once the check finishes (an extra working-set trim), so idle RAM stays where 0.3.0 was
- Binary size: ~9.9 MB → **~10.1 MB** (release, LTO). DBeaver password decryption reuses the `aes` / `cbc` crates already linked through the SSH stack — no new heavy dependencies

---

## 🧪 Testing

- 14 new unit tests — INSERT generation & quoting, single-table detection, DBeaver parsing & credential decryption, ALTER statement ordering, AI response parsing & write-operation detection, version comparison — run on every `cargo test`
- 3 new `#[ignore]`-gated integration tests (`cargo test -- --ignored`):
  - **Dump round-trip** — dumps a schema with every supported object kind, restores it under a new name on a live Postgres, and compares data, sequence positions, constraints, indexes and triggers
  - **Structure editor** — applies a generated ALTER batch on a live Postgres and verifies a failing batch leaves nothing half-applied
  - **Update check** — hits the real GitHub releases API

---

## 📦 Build Info

| | |
|---|---|
| **Version** | 0.3.1 |
| **Binary size** | ~10.1 MB (release LTO) |
| **RAM at idle** | ~40–55 MB working set (same as 0.3.0) |
| **Rust edition** | 2024 |
