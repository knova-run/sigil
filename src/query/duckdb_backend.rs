//! DuckDB-backed query engine — Phase 0.5 scale path (plan §14.9).
//!
//! Built lazily from sigil's existing JSONL source of truth
//! (`.sigil/entities.jsonl` + `.sigil/refs.jsonl`). When JSONL size
//! exceeds the auto-upgrade threshold (50 MB by default), or when the
//! caller asks for the DB backend explicitly, this module stands in for
//! the in-memory `Index` on the same query API.
//!
//! ## Why DuckDB
//!
//! See plan §14.9 for the full trade-off matrix. The short version: the
//! in-memory hash-map Index is great up to ~500k entities; above that,
//! cold-start JSONL parse + hash-map construction becomes painful.
//! DuckDB's zero-ETL `read_json_auto` + vectorized columnar engine
//! handles analytical queries (rank joins, blast-radius aggregates,
//! map-shaped ranked group-bys) 5–20× faster than a row-oriented store
//! at this scale, with a smaller memory footprint than keeping every
//! entity in RAM.
//!
//! ## Artifacts on disk
//!
//! - `.sigil/index.duckdb`        — the materialized database (gitignored)
//! - `.sigil/index.duckdb.stamp`  — JSONL mtime/size fingerprint; the DB
//!   rebuilds from scratch on any mismatch
//!
//! ## Lifecycle
//!
//! ```text
//!          ┌─────────────────────────┐
//!          │  DuckDbBackend::open()  │
//!          └──────────┬──────────────┘
//!                     │
//!             stamp matches JSONL?
//!                 ┌───┴───┐
//!                 │       │
//!                yes      no
//!                 │       ▼
//!                 │   rebuild from JSONL via read_json_auto
//!                 │       │
//!                 ▼       ▼
//!          ┌──────────────────┐
//!          │ ready for queries │
//!          └──────────────────┘
//! ```
//!
//! The stamp file stores bytes length + modified epoch for each JSONL
//! source. A size-only check would miss content-preserving edits
//! (impossible in practice but cheap to guard against).
//!
//! ## Feature gate
//!
//! Compiled only when `cargo build --features db`. Absent that, the
//! module is a type-free empty module and callers fall through to the
//! in-memory path unconditionally.
//!
//! ## Build requirements
//!
//! `--features db` pulls in `libduckdb-sys`, which bundles DuckDB's C++
//! source and compiles it with the host toolchain. A working C++17
//! toolchain + stdlib headers are required. On macOS that means Xcode
//! Command Line Tools (`xcode-select --install`); on Debian-ish Linux,
//! `apt install build-essential`; on Windows, MSVC or an MSYS2
//! toolchain. CI images typically have this already.
//!
//! If a build fails with `fatal error: 'memory' file not found` or
//! similar missing-stdlib messages, the host C++ toolchain is the
//! problem — not sigil. The fix is environmental.

#![cfg(feature = "db")]

use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use duckdb::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::entity::{Entity, Reference};
use crate::query::index::{DirSummary, FileHit, Scope};
use crate::query::SearchHitOwned;

/// Default auto-upgrade threshold in bytes.
///
/// Sigil's per-query cost on the in-memory backend is dominated by
/// JSONL load + hashmap build on every invocation (each `sigil <cmd>`
/// is a fresh process). Benchmarks (see `evals/results/multilang-…`)
/// show the crossover point where DuckDB's persistent materialized
/// store beats re-loading JSONL sits around ~5 MB total, not the
/// 50 MB originally conjectured.
///
/// Override via the `SIGIL_AUTO_ENGAGE_THRESHOLD_MB` env var when a
/// specific workload wants a different crossover; the env path
/// short-circuits `Backend::load` before this constant is consulted.
pub const DEFAULT_AUTO_UPGRADE_THRESHOLD_BYTES: u64 = 5 * 1024 * 1024;

/// Returns `true` when the DuckDB backend should engage by default for
/// the given `root` — used by callers that want transparent routing
/// rather than forcing a build config at compile time.
pub fn should_auto_engage(root: &Path, threshold_bytes: u64) -> bool {
    let total = [".sigil/entities.jsonl", ".sigil/refs.jsonl", ".sigil/files.jsonl"]
        .iter()
        .map(|p| std::fs::metadata(root.join(p)).map(|m| m.len()).unwrap_or(0))
        .sum::<u64>();
    total >= threshold_bytes
}

/// DuckDB-backed query engine. Opens (or rebuilds) the `.sigil/index.duckdb`
/// cache on construction; queries run against that materialized store.
pub struct DuckDbBackend {
    conn: Connection,
    root: PathBuf,
}

impl DuckDbBackend {
    /// Open the backend at `root/.sigil/index.duckdb`, rebuilding from
    /// JSONL if the stamp file is stale or missing.
    pub fn open(root: &Path) -> Result<Self> {
        let sigil_dir = root.join(".sigil");
        std::fs::create_dir_all(&sigil_dir)?;
        let db_path = sigil_dir.join("index.duckdb");
        let stamp_path = sigil_dir.join("index.duckdb.stamp");

        let expected = fingerprint(&sigil_dir);
        let actual = Stamp::load(&stamp_path).ok();

        // Rebuild when stamps diverge. Dropping the DB file entirely is
        // cheaper than TRUNCATE + re-import because DuckDB lays the file
        // out in its own format we don't control.
        let needs_rebuild = actual.as_ref() != Some(&expected);
        if needs_rebuild && db_path.exists() {
            std::fs::remove_file(&db_path).ok();
        }

        let conn = Connection::open(&db_path)
            .with_context(|| format!("open DuckDB at {}", db_path.display()))?;

        if needs_rebuild {
            populate(&conn, &sigil_dir)
                .context("rebuild DuckDB index from JSONL")?;
            expected.save(&stamp_path)?;
        }

        // Safety: a matching stamp doesn't guarantee populated tables —
        // a previous `populate()` could have been interrupted mid-run,
        // leaving an empty DB + a valid stamp. (Root cause of the silent
        // E2 regression where sigil_callers returned 100 bytes on every
        // treatment call.) When JSONL has content but both tables are
        // empty, force one rebuild. Cheap — a no-op on healthy indexes.
        let tables_empty = count_tables(&conn)
            .map(|(e, r)| e == 0 && r == 0)
            .unwrap_or(true);
        let jsonl_has_content = std::fs::metadata(sigil_dir.join("entities.jsonl"))
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        if tables_empty && jsonl_has_content {
            eprintln!(
                "sigil: .sigil/index.duckdb has empty tables but JSONL is populated — rebuilding."
            );
            drop(conn);
            std::fs::remove_file(&db_path).ok();
            let conn = Connection::open(&db_path)
                .with_context(|| format!("reopen DuckDB at {}", db_path.display()))?;
            populate(&conn, &sigil_dir)
                .context("recovery rebuild of DuckDB index")?;
            fingerprint(&sigil_dir).save(&stamp_path)?;
            return Ok(Self {
                conn,
                root: root.to_path_buf(),
            });
        }

        Ok(Self {
            conn,
            root: root.to_path_buf(),
        })
    }

    /// Total `(entities, references)` counts — cheap sanity check.
    pub fn len(&self) -> Result<(usize, usize)> {
        let entities: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))?;
        let refs: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM refs", [], |r| r.get(0))?;
        Ok((entities as usize, refs as usize))
    }

    /// Callers of `name`, in (file, line) order for stable output.
    /// `limit == 0` → unlimited. A bare `name` also matches refs whose
    /// stored name is a `::`-qualified path ending in `::name` (parity
    /// with `Index::build`'s qualified-tail indexing).
    pub fn get_callers(
        &self,
        name: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Reference>> {
        let want_qualified = !name.contains("::");
        let like_pattern = format!("%::{}", name);
        let mut sql = String::from(
            "SELECT file, caller, name, kind, line \
             FROM refs \
             WHERE (name = ?",
        );
        if want_qualified {
            sql.push_str(" OR name LIKE ?");
        }
        sql.push(')');
        if kind_filter.is_some() {
            sql.push_str(" AND kind = ?");
        }
        sql.push_str(" ORDER BY file, line");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = match (want_qualified, kind_filter) {
            (true, Some(k)) => stmt
                .query_map(params![name, like_pattern, k], row_to_reference)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            (true, None) => stmt
                .query_map(params![name, like_pattern], row_to_reference)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            (false, Some(k)) => stmt
                .query_map(params![name, k], row_to_reference)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            (false, None) => stmt
                .query_map(params![name], row_to_reference)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    /// Callees of `caller` — refs whose `caller` column equals `caller`.
    /// Mirrors `Index::get_callees`. Dedupe happens implicitly at the
    /// index level since refs carry `(file, line)` as a natural key.
    pub fn get_callees(
        &self,
        caller: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Reference>> {
        let mut sql = String::from(
            "SELECT file, caller, name, kind, line \
             FROM refs \
             WHERE caller = ?",
        );
        if kind_filter.is_some() {
            sql.push_str(" AND kind = ?");
        }
        sql.push_str(" ORDER BY file, line");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = if let Some(k) = kind_filter {
            stmt.query_map(params![caller, k], row_to_reference)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![caller], row_to_reference)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    /// All entities in `file`, optionally filtered by kind. Ordered by
    /// `line_start` so successive calls return the same prefix — stable
    /// behavior callers depend on for pagination.
    ///
    /// Returns sigil `Entity` rows; the DuckDB backend only hydrates
    /// scalar columns (no `meta`, `rank`, or `blast_radius`). Consumers
    /// needing those fields should load the in-memory `Index`, which
    /// carries the full struct. Documented on
    /// [`populate_entity_from_row`] below.
    pub fn get_file_symbols(
        &self,
        file: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>> {
        let mut sql = String::from(
            "SELECT file, name, kind, line_start, line_end, parent, qualified_name, sig, \
                    visibility, body_hash, sig_hash, struct_hash \
             FROM entities \
             WHERE file = ?",
        );
        if kind_filter.is_some() {
            sql.push_str(" AND kind = ?");
        }
        sql.push_str(" ORDER BY line_start");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = if let Some(k) = kind_filter {
            stmt.query_map(params![file, k], row_to_entity)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![file], row_to_entity)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    /// Children of `(file, parent)` — entities whose `parent` column
    /// matches. Same column set + limitations as `get_file_symbols`.
    pub fn get_children(
        &self,
        file: &str,
        parent: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>> {
        let mut sql = String::from(
            "SELECT file, name, kind, line_start, line_end, parent, qualified_name, sig, \
                    visibility, body_hash, sig_hash, struct_hash \
             FROM entities \
             WHERE file = ? AND parent = ?",
        );
        if kind_filter.is_some() {
            sql.push_str(" AND kind = ?");
        }
        sql.push_str(" ORDER BY line_start");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = if let Some(k) = kind_filter {
            stmt.query_map(params![file, parent, k], row_to_entity)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![file, parent], row_to_entity)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    /// Full search matching `Index::search` semantics across all three
    /// `Scope` variants. Symbol matches hit entity names;
    /// file matches hit file paths via the same case-insensitive
    /// substring rule. Empty queries short-circuit to `Vec::new()`.
    pub fn search(
        &self,
        query: &str,
        scope: Scope,
        kind_filter: Option<&str>,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHitOwned>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let want_symbols = matches!(scope, Scope::All | Scope::Symbols);
        let want_files = matches!(scope, Scope::All | Scope::Files);

        let mut hits: Vec<SearchHitOwned> = Vec::new();

        if want_symbols {
            for e in self.search_symbols_impl(query, kind_filter, path_prefix, remaining_limit(limit, hits.len()))? {
                hits.push(SearchHitOwned::Symbol(e));
                if limit > 0 && hits.len() >= limit {
                    return Ok(hits);
                }
            }
        }

        if want_files {
            for f in self.search_files_impl(query, path_prefix, remaining_limit(limit, hits.len()))? {
                hits.push(SearchHitOwned::File(f));
                if limit > 0 && hits.len() >= limit {
                    break;
                }
            }
        }

        Ok(hits)
    }

    fn search_symbols_impl(
        &self,
        query: &str,
        kind_filter: Option<&str>,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>> {
        let needle = format!("%{}%", query.to_lowercase());
        let mut sql = String::from(
            "SELECT file, name, kind, line_start, line_end, parent, qualified_name, sig, \
                    visibility, body_hash, sig_hash, struct_hash \
             FROM entities \
             WHERE lower(name) LIKE ?",
        );
        if kind_filter.is_some() {
            sql.push_str(" AND kind = ?");
        }
        if path_prefix.is_some() {
            sql.push_str(" AND file LIKE ?");
        }
        sql.push_str(" ORDER BY file, line_start");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = match (kind_filter, path_prefix) {
            (Some(k), Some(p)) => stmt
                .query_map(params![needle, k, format!("{p}%")], row_to_entity)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            (Some(k), None) => stmt
                .query_map(params![needle, k], row_to_entity)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            (None, Some(p)) => stmt
                .query_map(params![needle, format!("{p}%")], row_to_entity)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            (None, None) => stmt
                .query_map(params![needle], row_to_entity)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    fn search_files_impl(
        &self,
        query: &str,
        path_prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FileHit>> {
        let needle = format!("%{}%", query.to_lowercase());
        // Subquery DISTINCT-scans the entities table since sigil doesn't
        // yet maintain a separate files table; every file with at least
        // one indexed entity is a candidate. `entity_count` comes from
        // a GROUP BY so callers see the richer row shape `FileHit`
        // carries.
        let mut sql = String::from(
            "SELECT file, COUNT(*) as entity_count \
             FROM entities \
             WHERE lower(file) LIKE ?",
        );
        if path_prefix.is_some() {
            sql.push_str(" AND file LIKE ?");
        }
        sql.push_str(" GROUP BY file ORDER BY file");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<(String, i64)> = if let Some(p) = path_prefix {
            stmt.query_map(params![needle, format!("{p}%")], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![needle], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(rows
            .into_iter()
            .map(|(path, count)| FileHit {
                lang: lang_for(&path).map(|s| s.to_string()),
                path,
                entity_count: count as usize,
            })
            .collect())
    }

    /// Directory overview — one `DirSummary` per unique directory in the
    /// indexed file set, with the languages present in each. Matches
    /// `Index::explore_dir_overview`.
    ///
    /// The grouping happens in Rust rather than SQL because directory
    /// extraction would require reverse-string slicing in SQL, which is
    /// awkward + backend-specific. Loading distinct file paths is cheap
    /// (one DISTINCT scan); the CPU grouping is trivial on the returned
    /// list.
    pub fn explore_dir_overview(&self, path_prefix: Option<&str>) -> Result<Vec<DirSummary>> {
        let files = self.distinct_files(path_prefix)?;
        let mut by_dir: BTreeMap<String, (usize, std::collections::BTreeSet<String>)> =
            BTreeMap::new();
        for f in &files {
            let dir = parent_dir(f).to_string();
            let entry = by_dir.entry(dir).or_default();
            entry.0 += 1;
            if let Some(lang) = lang_for(f) {
                entry.1.insert(lang.to_string());
            }
        }
        Ok(by_dir
            .into_iter()
            .map(|(path, (file_count, langs))| DirSummary {
                path,
                file_count,
                langs: langs.into_iter().collect(),
            })
            .collect())
    }

    /// Flat file listing capped per-directory. Same shape as
    /// `Index::explore_files_capped` so the router can swap backends.
    pub fn explore_files_capped(
        &self,
        path_prefix: Option<&str>,
        cap_per_dir: usize,
    ) -> Result<Vec<(String, String, Option<String>)>> {
        let mut files = self.distinct_files(path_prefix)?;
        files.sort();

        let mut by_dir: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for f in files {
            let dir = parent_dir(&f).to_string();
            by_dir.entry(dir).or_default().push(f);
        }
        let mut out = Vec::new();
        for (dir, mut entries) in by_dir {
            entries.sort();
            if cap_per_dir > 0 && entries.len() > cap_per_dir {
                entries.truncate(cap_per_dir);
            }
            for f in entries {
                let lang = lang_for(&f).map(|s| s.to_string());
                out.push((dir.clone(), f, lang));
            }
        }
        Ok(out)
    }

    fn distinct_files(&self, path_prefix: Option<&str>) -> Result<Vec<String>> {
        let mut sql = String::from("SELECT DISTINCT file FROM entities");
        if path_prefix.is_some() {
            sql.push_str(" WHERE file LIKE ?");
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<String> = if let Some(p) = path_prefix {
            stmt.query_map(params![format!("{p}%")], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    /// Sigil operates in single-project mode — the empty-string project
    /// is the whole tree. `Index::list_projects` returns `vec![""]` for
    /// compatibility with pre-decodeix call sites that expected the
    /// codeix MountTable convention; we mirror it here.
    pub fn list_projects(&self) -> Result<Vec<String>> {
        Ok(vec![String::new()])
    }

    /// Where the DuckDB store lives on disk. Exposed for consumers that
    /// want to run ad-hoc SQL against the same database.
    pub fn db_path(&self) -> PathBuf {
        self.root.join(".sigil/index.duckdb")
    }

    /// Execute ad-hoc SQL and return the result as a column-labeled
    /// table. Powers `sigil query 'SQL'`. Read-only in spirit — we
    /// don't block DDL but also don't document it; mutating the
    /// materialized store out from under sigil is at the user's risk
    /// since the next staleness-triggered rebuild will blow it away.
    pub fn exec_query(&self, sql: &str) -> Result<QueryResult> {
        let mut stmt = self.conn.prepare(sql)?;
        // `column_names()` reads the schema set up during `query()` —
        // call query() first, then extract column names, then iterate.
        // Calling column_names() on a prepared-but-not-yet-executed
        // statement panics inside duckdb-rs (`schema.unwrap()`).
        let mut it = stmt.query([])?;
        let columns: Vec<String> = it
            .as_ref()
            .map(|s| s.column_names().into_iter().map(String::from).collect())
            .unwrap_or_default();
        let col_count = columns.len();
        let mut rows: Vec<Vec<String>> = Vec::new();
        while let Some(row) = it.next()? {
            let mut r = Vec::with_capacity(col_count);
            for i in 0..col_count {
                r.push(format_cell(row, i));
            }
            rows.push(r);
        }
        Ok(QueryResult { columns, rows })
    }
}

/// Tabular SQL result. Owned so the CLI layer can outlive the
/// `DuckDbBackend` connection borrow.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl QueryResult {
    /// Render as a pipe-delimited markdown table. Truncates each cell to
    /// `max_cell_width` chars with `…` so long strings don't break
    /// terminal line wrap. `0` disables truncation.
    pub fn to_markdown(&self, max_cell_width: usize) -> String {
        let truncate = |s: &str| -> String {
            if max_cell_width == 0 || s.chars().count() <= max_cell_width {
                s.to_string()
            } else {
                let mut out: String = s.chars().take(max_cell_width.saturating_sub(1)).collect();
                out.push('…');
                out
            }
        };
        let mut out = String::with_capacity(1024);
        if self.columns.is_empty() {
            return "_(no columns)_\n".to_string();
        }
        out.push_str("| ");
        out.push_str(
            &self
                .columns
                .iter()
                .map(|c| truncate(c))
                .collect::<Vec<_>>()
                .join(" | "),
        );
        out.push_str(" |\n|");
        for _ in &self.columns {
            out.push_str("---|");
        }
        out.push('\n');
        for row in &self.rows {
            out.push_str("| ");
            out.push_str(
                &row.iter()
                    .map(|c| truncate(c))
                    .collect::<Vec<_>>()
                    .join(" | "),
            );
            out.push_str(" |\n");
        }
        out.push_str(&format!("\n_{} row(s)_\n", self.rows.len()));
        out
    }

    /// Render as JSON — a list of objects keyed by column name. Strings
    /// in every cell; numeric / boolean values come out as string-wrapped
    /// "42" for uniformity (the CLI is for exploration, not downstream
    /// typed pipelines).
    pub fn to_json(&self, pretty: bool) -> String {
        let objs: Vec<serde_json::Map<String, serde_json::Value>> = self
            .rows
            .iter()
            .map(|row| {
                let mut m = serde_json::Map::new();
                for (i, col) in self.columns.iter().enumerate() {
                    let v = row.get(i).cloned().unwrap_or_default();
                    m.insert(col.clone(), serde_json::Value::String(v));
                }
                m
            })
            .collect();
        if pretty {
            serde_json::to_string_pretty(&objs).unwrap_or_default()
        } else {
            serde_json::to_string(&objs).unwrap_or_default()
        }
    }
}

/// Best-effort cell stringifier. DuckDB returns a `ValueRef` per cell;
/// we pattern-match the variants we expect to see (text, numeric,
/// boolean, NULL) and fall through to a fallback for anything exotic
/// (lists, structs, blobs) which sigil shouldn't produce in its own
/// tables but might show up in user SQL.
fn format_cell(row: &duckdb::Row<'_>, idx: usize) -> String {
    use duckdb::types::ValueRef;
    match row.get_ref(idx) {
        Ok(ValueRef::Null) => String::new(),
        Ok(ValueRef::Boolean(b)) => b.to_string(),
        Ok(ValueRef::TinyInt(v)) => v.to_string(),
        Ok(ValueRef::SmallInt(v)) => v.to_string(),
        Ok(ValueRef::Int(v)) => v.to_string(),
        Ok(ValueRef::BigInt(v)) => v.to_string(),
        Ok(ValueRef::HugeInt(v)) => v.to_string(),
        Ok(ValueRef::UTinyInt(v)) => v.to_string(),
        Ok(ValueRef::USmallInt(v)) => v.to_string(),
        Ok(ValueRef::UInt(v)) => v.to_string(),
        Ok(ValueRef::UBigInt(v)) => v.to_string(),
        Ok(ValueRef::Float(v)) => v.to_string(),
        Ok(ValueRef::Double(v)) => v.to_string(),
        Ok(ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Ok(ValueRef::Blob(_)) => "<blob>".to_string(),
        Ok(other) => format!("{other:?}"),
        Err(e) => format!("<error: {e}>"),
    }
}

// ---- internals ----

fn populate(conn: &Connection, sigil_dir: &Path) -> Result<()> {
    let entities_path = sigil_dir.join("entities.jsonl");
    let refs_path = sigil_dir.join("refs.jsonl");

    // `sigil index` only writes refs.jsonl when refs are non-empty, so the
    // file can legitimately be absent. Build each table conditionally
    // against read_json_auto when the source exists; otherwise create an
    // empty table with the right column shape so downstream queries don't
    // fail with "table not found." Same idea for entities — a freshly
    // scaffolded .sigil/ with no entities shouldn't crash here.
    // Use `read_json` with an explicit column spec rather than
    // `read_json_auto`. Auto-inference only picks up fields that appear
    // in sampled rows — which means optional fields missing from every
    // row (e.g., `parent` on a set of top-level entities) get silently
    // dropped, and subsequent SELECTs fail with "column not found".
    // Explicit columns make the schema stable regardless of which
    // optional fields happen to be populated.
    let entities_sql = if entities_path.exists() {
        format!(
            "CREATE TABLE entities AS SELECT * FROM read_json('{}', columns = {});",
            path_for_sql(&entities_path),
            ENTITIES_COLUMNS_SPEC,
        )
    } else {
        empty_entities_table_sql().to_string()
    };
    let refs_sql = if refs_path.exists() {
        format!(
            "CREATE TABLE refs AS SELECT * FROM read_json('{}', columns = {});",
            path_for_sql(&refs_path),
            REFS_COLUMNS_SPEC,
        )
    } else {
        empty_refs_table_sql().to_string()
    };

    // Materialize into real tables (not views) so queries don't re-parse
    // JSONL on every call. Rebuild is cheap — zero-ETL via read_json_auto.
    conn.execute_batch(&format!(
        "{entities_sql}
         {refs_sql}
         CREATE INDEX idx_entities_name ON entities(name);
         CREATE INDEX idx_entities_file ON entities(file);
         CREATE INDEX idx_refs_name   ON refs(name);
         CREATE INDEX idx_refs_caller ON refs(caller);
         CREATE INDEX idx_refs_file   ON refs(file);",
    ))
    .context("populate entities/refs tables + indexes")?;
    Ok(())
}

/// Column shape for the entities table when JSONL is missing. Mirrors
/// the Entity struct's serialized fields so schemas are compatible
/// when a real JSONL arrives later (but the DB rebuilds on staleness
/// anyway, so exact match isn't strictly required).
fn empty_entities_table_sql() -> &'static str {
    "CREATE TABLE entities (
        file VARCHAR, name VARCHAR, kind VARCHAR,
        line_start BIGINT, line_end BIGINT,
        parent VARCHAR, qualified_name VARCHAR,
        sig VARCHAR, meta VARCHAR,
        body_hash VARCHAR, sig_hash VARCHAR, struct_hash VARCHAR,
        visibility VARCHAR, rank DOUBLE, blast_radius VARCHAR,
        doc VARCHAR
    );"
}

fn empty_refs_table_sql() -> &'static str {
    "CREATE TABLE refs (
        file VARCHAR, caller VARCHAR, name VARCHAR,
        kind VARCHAR, line BIGINT
    );"
}

/// Explicit column specs passed to `read_json(..., columns = ...)`.
/// Covers every Entity / Reference field sigil may emit so optional
/// fields missing from the input rows still materialize as NULL in
/// the table rather than causing "column not found" errors.
///
/// `meta` is a list in Rust (`Option<Vec<String>>`); we read it back
/// as JSON text to avoid DuckDB LIST handling in every query site that
/// doesn't consume it. `blast_radius` is a struct; likewise read as
/// JSON text for now. Neither is surfaced by the DuckDB-backed query
/// methods today — consumers that need the typed forms should load
/// the in-memory Index.
const ENTITIES_COLUMNS_SPEC: &str = "{ \
    file: 'VARCHAR', \
    name: 'VARCHAR', \
    kind: 'VARCHAR', \
    line_start: 'BIGINT', \
    line_end: 'BIGINT', \
    parent: 'VARCHAR', \
    qualified_name: 'VARCHAR', \
    sig: 'VARCHAR', \
    meta: 'JSON', \
    body_hash: 'VARCHAR', \
    sig_hash: 'VARCHAR', \
    struct_hash: 'VARCHAR', \
    visibility: 'VARCHAR', \
    rank: 'DOUBLE', \
    blast_radius: 'JSON', \
    doc: 'VARCHAR' \
}";

const REFS_COLUMNS_SPEC: &str = "{ \
    file: 'VARCHAR', \
    caller: 'VARCHAR', \
    name: 'VARCHAR', \
    kind: 'VARCHAR', \
    line: 'BIGINT' \
}";

fn row_to_reference(row: &duckdb::Row<'_>) -> duckdb::Result<Reference> {
    Ok(Reference {
        file: row.get::<_, String>(0)?,
        caller: row.get::<_, Option<String>>(1)?,
        name: row.get::<_, String>(2)?,
        ref_kind: row.get::<_, String>(3)?,
        line: row.get::<_, i64>(4)? as u32,
    })
}

/// Remaining quota when packing mixed search hits. `limit == 0` means
/// unlimited on the caller's side; we pass `0` straight through so SQL
/// doesn't cap the inner query.
fn remaining_limit(total: usize, so_far: usize) -> usize {
    if total == 0 {
        0
    } else {
        total.saturating_sub(so_far)
    }
}

/// Directory component of a path, or `""` for the root. Mirrors
/// `query::index::parent_dir` so the two backends agree on
/// "(what bucket does this file live in?)".
fn parent_dir(file: &str) -> &str {
    match file.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Language name for a file, if sigil parses it. Extends the vendored
/// tree-sitter detector with sigil's four native formats
/// (json / yaml / toml / markdown). Matches `Index`'s helper so the
/// two backends label files identically.
fn lang_for(file: &str) -> Option<&'static str> {
    let ext = file.rsplit_once('.').map(|(_, e)| e)?;
    if let Some(lang) = crate::parser::languages::detect_language(ext) {
        return Some(lang);
    }
    match ext {
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        _ => None,
    }
}

/// Hydrate the subset of `Entity` that the DuckDB backend extracts —
/// scalar columns only. `meta`, `rank`, `blast_radius`, and `doc` stay
/// `None` because reading them back requires DuckDB STRUCT/LIST parsing
/// (or, for `doc`, just isn't surfaced by today's query methods). Any
/// consumer that needs the full struct should load the in-memory `Index`
/// (which parses JSONL directly into the Rust struct and keeps every
/// field).
///
/// Column order must match the SELECT lists in the methods above:
/// file, name, kind, line_start, line_end, parent, qualified_name, sig,
/// visibility, body_hash, sig_hash, struct_hash.
fn row_to_entity(row: &duckdb::Row<'_>) -> duckdb::Result<Entity> {
    Ok(Entity {
        file: row.get::<_, String>(0)?,
        name: row.get::<_, String>(1)?,
        kind: row.get::<_, String>(2)?,
        line_start: row.get::<_, i64>(3)? as u32,
        line_end: row.get::<_, i64>(4)? as u32,
        parent: row.get::<_, Option<String>>(5)?,
        qualified_name: row.get::<_, Option<String>>(6)?,
        sig: row.get::<_, Option<String>>(7)?,
        meta: None,
        body_hash: row.get::<_, Option<String>>(9)?,
        sig_hash: row.get::<_, Option<String>>(10)?,
        struct_hash: row.get::<_, String>(11)?,
        visibility: row.get::<_, Option<String>>(8)?,
        rank: None,
        blast_radius: None,
        doc: None,
    })
}

/// DuckDB's SQL expects `'...'` strings; we single-quote by escaping any
/// embedded quotes. The paths we pass are absolute sigil-controlled
/// locations, so injection isn't a real risk — this is just correctness.
fn path_for_sql(p: &Path) -> String {
    p.display().to_string().replace('\'', "''")
}

/// Fingerprint of the JSONL files the DB was built from. Captured at
/// build time and compared on next open to decide whether to rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    entities_len: u64,
    entities_mtime_ms: u128,
    refs_len: u64,
    refs_mtime_ms: u128,
}

impl Stamp {
    fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&text).map_err(Into::into)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string(self)?;
        std::fs::write(path, text).map_err(Into::into)
    }
}

/// Count rows in the entities and refs tables. Used by `open()`'s
/// empty-tables-but-valid-stamp recovery path. Returns None if either
/// table is absent (fresh DB where populate hasn't run yet).
fn count_tables(conn: &Connection) -> Option<(i64, i64)> {
    let ents: i64 = conn
        .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
        .ok()?;
    let refs: i64 = conn
        .query_row("SELECT COUNT(*) FROM refs", [], |r| r.get(0))
        .ok()?;
    Some((ents, refs))
}

fn fingerprint(sigil_dir: &Path) -> Stamp {
    let (entities_len, entities_mtime_ms) = meta_pair(&sigil_dir.join("entities.jsonl"));
    let (refs_len, refs_mtime_ms) = meta_pair(&sigil_dir.join("refs.jsonl"));
    Stamp {
        entities_len,
        entities_mtime_ms,
        refs_len,
        refs_mtime_ms,
    }
}

fn meta_pair(p: &Path) -> (u64, u128) {
    let Ok(m) = std::fs::metadata(p) else {
        return (0, 0);
    };
    let len = m.len();
    let mtime_ms = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    (len, mtime_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{Entity, Reference};
    use crate::writer;

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("sigil_duckdb_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn ent(file: &str, name: &str, kind: &str) -> Entity {
        Entity {
            file: file.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            line_start: 1,
            line_end: 2,
            parent: None,
            qualified_name: None,
            sig: None,
            meta: None,
            body_hash: Some("d".to_string()),
            sig_hash: None,
            struct_hash: "s".to_string(),
            visibility: None,
            rank: None,
            blast_radius: None,
            doc: None,
        }
    }

    fn refr(file: &str, caller: Option<&str>, name: &str, kind: &str, line: u32) -> Reference {
        Reference {
            file: file.to_string(),
            caller: caller.map(str::to_string),
            name: name.to_string(),
            ref_kind: kind.to_string(),
            line,
        }
    }

    fn seed(root: &Path, entities: Vec<Entity>, refs: Vec<Reference>) {
        writer::write_to_files(&entities, &refs, root, false).unwrap();
    }

    #[test]
    fn opens_and_populates_from_jsonl() {
        let root = tmpdir("populate");
        seed(
            &root,
            vec![ent("a.rs", "Foo", "struct"), ent("b.rs", "bar", "function")],
            vec![refr("a.rs", Some("caller"), "bar", "call", 10)],
        );
        let db = DuckDbBackend::open(&root).expect("open");
        assert_eq!(db.len().unwrap(), (2, 1));
        assert!(root.join(".sigil/index.duckdb").exists());
        assert!(root.join(".sigil/index.duckdb.stamp").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stamp_match_skips_rebuild() {
        let root = tmpdir("cached");
        seed(
            &root,
            vec![ent("a.rs", "Foo", "struct")],
            vec![refr("b.rs", Some("c"), "Foo", "type_annotation", 5)],
        );
        let _ = DuckDbBackend::open(&root).unwrap();
        let db_mtime_first = std::fs::metadata(root.join(".sigil/index.duckdb"))
            .unwrap()
            .modified()
            .unwrap();
        // Small sleep to ensure the filesystem mtime could differ if we
        // were to rewrite. A proper clock-skew-tolerant test would check
        // a monotonic counter, but this is sufficient for local runs.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = DuckDbBackend::open(&root).unwrap();
        let db_mtime_second = std::fs::metadata(root.join(".sigil/index.duckdb"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(db_mtime_first, db_mtime_second, "DB should not be rewritten");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stamp_mismatch_triggers_rebuild() {
        let root = tmpdir("stale");
        seed(
            &root,
            vec![ent("a.rs", "Foo", "struct")],
            vec![refr("b.rs", Some("c"), "Foo", "call", 5)],
        );
        let first = DuckDbBackend::open(&root).unwrap();
        assert_eq!(first.len().unwrap(), (1, 1));
        drop(first);

        // Re-seed with more data — stamp's (size, mtime) will differ and
        // force a rebuild.
        std::thread::sleep(std::time::Duration::from_millis(10));
        seed(
            &root,
            vec![
                ent("a.rs", "Foo", "struct"),
                ent("c.rs", "Bar", "struct"),
                ent("d.rs", "Baz", "struct"),
            ],
            vec![
                refr("b.rs", Some("c"), "Foo", "call", 5),
                refr("b.rs", Some("c"), "Bar", "call", 6),
            ],
        );
        let second = DuckDbBackend::open(&root).unwrap();
        assert_eq!(second.len().unwrap(), (3, 2), "DB should reflect new JSONL");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn get_callers_matches_in_memory_semantics() {
        let root = tmpdir("callers");
        seed(
            &root,
            vec![ent("tgt.rs", "Foo", "struct")],
            vec![
                refr("a.rs", Some("user"), "Foo", "type_annotation", 1),
                refr("b.rs", Some("user"), "Foo", "call", 2),
                refr("c.rs", Some("user"), "Foo", "call", 3),
                refr("d.rs", Some("user"), "Other", "call", 4),
            ],
        );
        let db = DuckDbBackend::open(&root).unwrap();

        let all = db.get_callers("Foo", None, 0).unwrap();
        assert_eq!(all.len(), 3);

        let filtered = db.get_callers("Foo", Some("call"), 0).unwrap();
        assert_eq!(filtered.len(), 2);

        let limited = db.get_callers("Foo", None, 2).unwrap();
        assert_eq!(limited.len(), 2);

        let missing = db.get_callers("Nonexistent", None, 0).unwrap();
        assert!(missing.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn get_callers_parity_with_in_memory_backend() {
        // Load the same JSONL into both backends; get_callers answers
        // should match row-for-row (modulo insertion order vs DuckDB's
        // file/line ordering — we sort both sides to canonicalize).
        let root = tmpdir("parity");
        seed(
            &root,
            vec![ent("tgt.rs", "Foo", "struct")],
            (0..12)
                .map(|i| refr(&format!("c{i}.rs"), Some("m"), "Foo", "call", i + 1))
                .collect(),
        );

        let db = DuckDbBackend::open(&root).unwrap();
        let idx = crate::query::index::Index::load(&root).unwrap();

        let mut from_db = db.get_callers("Foo", None, 0).unwrap();
        let mut from_idx: Vec<Reference> = idx
            .get_callers("Foo", None, 0)
            .into_iter()
            .cloned()
            .collect();
        from_db.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));
        from_idx.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.line.cmp(&b.line)));
        assert_eq!(from_db, from_idx);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn get_callers_matches_qualified_tail() {
        // Parity with `Index::build`: a bare-name search (`foo`) must also
        // surface refs whose stored name is a `::`-qualified path ending in
        // `::foo`. Prevents the src/index.rs-calls-crate::parser::tree
        // sitter::parse_file regression from coming back under DuckDB.
        let root = tmpdir("qualified_callers");
        seed(
            &root,
            vec![ent("a.rs", "foo", "function")],
            vec![
                refr("b.rs", Some("main"), "foo", "call", 1),
                refr("c.rs", Some("caller"), "crate::a::b::foo", "call", 2),
                refr("d.rs", Some("caller"), "Foo::foo", "call", 3),
                refr("e.rs", Some("caller"), "bar", "call", 4),     // no match
                refr("f.rs", Some("caller"), "foobar", "call", 5), // no match (no `::` boundary)
            ],
        );
        let db = DuckDbBackend::open(&root).unwrap();

        let bare = db.get_callers("foo", None, 0).unwrap();
        assert_eq!(bare.len(), 3, "bare `foo` matches plain + both qualified refs");

        let qualified = db.get_callers("crate::a::b::foo", None, 0).unwrap();
        assert_eq!(qualified.len(), 1);

        let miss = db.get_callers("baz", None, 0).unwrap();
        assert!(miss.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn should_auto_engage_honors_threshold() {
        let root = tmpdir("threshold");
        seed(
            &root,
            vec![ent("a.rs", "Foo", "struct")],
            vec![refr("b.rs", Some("c"), "Foo", "call", 1)],
        );
        assert!(!should_auto_engage(&root, 50 * 1024 * 1024));
        // Tiny threshold → even the one-entity fixture flips the gate.
        assert!(should_auto_engage(&root, 1));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn get_callees_mirrors_caller_column_lookup() {
        let root = tmpdir("callees");
        seed(
            &root,
            vec![ent("a.rs", "main", "function")],
            vec![
                refr("a.rs", Some("main"), "foo", "call", 1),
                refr("a.rs", Some("main"), "bar", "call", 2),
                refr("a.rs", Some("helper"), "foo", "call", 3),
            ],
        );
        let db = DuckDbBackend::open(&root).unwrap();
        let from_main = db.get_callees("main", None, 0).unwrap();
        assert_eq!(from_main.len(), 2);
        let from_helper = db.get_callees("helper", None, 0).unwrap();
        assert_eq!(from_helper.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn get_file_symbols_returns_entities_ordered_by_line() {
        let root = tmpdir("file_symbols");
        let mut a = ent("a.rs", "Foo", "struct");
        a.line_start = 10;
        let mut b = ent("a.rs", "bar", "function");
        b.line_start = 3;
        let mut c = ent("b.rs", "other", "function");
        c.line_start = 1;
        seed(&root, vec![a, b, c], vec![]);

        let db = DuckDbBackend::open(&root).unwrap();
        let in_a = db.get_file_symbols("a.rs", None, 0).unwrap();
        assert_eq!(in_a.len(), 2);
        assert_eq!(in_a[0].name, "bar", "line 3 sorts before line 10");
        assert_eq!(in_a[1].name, "Foo");

        let only_structs = db.get_file_symbols("a.rs", Some("struct"), 0).unwrap();
        assert_eq!(only_structs.len(), 1);
        assert_eq!(only_structs[0].name, "Foo");

        let missing = db.get_file_symbols("nonexistent.rs", None, 0).unwrap();
        assert!(missing.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn get_children_filters_by_parent() {
        let root = tmpdir("children");
        let mut m1 = ent("a.rs", "method_one", "method");
        m1.parent = Some("Foo".to_string());
        m1.line_start = 5;
        let mut m2 = ent("a.rs", "method_two", "method");
        m2.parent = Some("Foo".to_string());
        m2.line_start = 10;
        let mut m3 = ent("a.rs", "other", "method");
        m3.parent = Some("Bar".to_string());
        seed(&root, vec![m1, m2, m3], vec![]);

        let db = DuckDbBackend::open(&root).unwrap();
        let foo_methods = db.get_children("a.rs", "Foo", None, 0).unwrap();
        assert_eq!(foo_methods.len(), 2);
        assert!(foo_methods.iter().all(|e| e.parent.as_deref() == Some("Foo")));
        let bar_methods = db.get_children("a.rs", "Bar", None, 0).unwrap();
        assert_eq!(bar_methods.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn search_symbols_case_insensitive_with_path_prefix() {
        let root = tmpdir("search_symbols");
        seed(
            &root,
            vec![
                ent("src/a.rs", "ParseError", "struct"),
                ent("src/a.rs", "parse", "function"),
                ent("tests/parse_test.rs", "parse", "function"),
            ],
            vec![],
        );
        let db = DuckDbBackend::open(&root).unwrap();

        // Scope::Symbols: case-insensitive name match.
        let all = db.search("PARSE", Scope::Symbols, None, None, 0).unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|h| matches!(h, SearchHitOwned::Symbol(_))));

        // Filter by kind.
        let only_fns = db.search("parse", Scope::Symbols, Some("function"), None, 0).unwrap();
        assert_eq!(only_fns.len(), 2);

        // Filter by path prefix.
        let src_only = db.search("parse", Scope::Symbols, None, Some("src/"), 0).unwrap();
        assert_eq!(src_only.len(), 2);

        // Empty query short-circuits.
        let empty = db.search("", Scope::All, None, None, 0).unwrap();
        assert!(empty.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn search_files_scope_returns_file_hits_only() {
        let root = tmpdir("search_files");
        seed(
            &root,
            vec![
                ent("src/parser/lib.rs", "parse_fn", "function"),
                ent("src/other.rs", "unrelated", "function"),
            ],
            vec![],
        );
        let db = DuckDbBackend::open(&root).unwrap();
        let hits = db.search("parser", Scope::Files, None, None, 0).unwrap();
        assert_eq!(hits.len(), 1);
        match &hits[0] {
            SearchHitOwned::File(f) => {
                assert_eq!(f.path, "src/parser/lib.rs");
                assert_eq!(f.lang.as_deref(), Some("rust"));
            }
            other => panic!("expected File hit, got {other:?}"),
        }

        // Scope::All combines both — same query should surface the symbol
        // inside `parse_fn` and the file match for `src/parser/lib.rs`.
        let combined = db.search("parse", Scope::All, None, None, 0).unwrap();
        let n_symbols = combined
            .iter()
            .filter(|h| matches!(h, SearchHitOwned::Symbol(_)))
            .count();
        let n_files = combined
            .iter()
            .filter(|h| matches!(h, SearchHitOwned::File(_)))
            .count();
        assert!(n_symbols >= 1);
        assert!(n_files >= 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn explore_dir_overview_groups_by_directory_with_langs() {
        let root = tmpdir("explore_overview");
        seed(
            &root,
            vec![
                ent("src/a.rs", "x", "function"),
                ent("src/b.rs", "y", "function"),
                ent("tests/t.rs", "t", "function"),
                ent("README.md", "r", "section"),
            ],
            vec![],
        );
        let db = DuckDbBackend::open(&root).unwrap();
        let overview = db.explore_dir_overview(None).unwrap();
        let by_path: std::collections::HashMap<String, &DirSummary> =
            overview.iter().map(|d| (d.path.clone(), d)).collect();
        assert_eq!(by_path.get("src").unwrap().file_count, 2);
        assert_eq!(by_path.get("tests").unwrap().file_count, 1);
        assert!(by_path.contains_key(""), "root-level files land in the empty-string dir");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn explore_files_capped_caps_per_directory() {
        let root = tmpdir("explore_cap");
        seed(
            &root,
            (0..10)
                .map(|i| ent(&format!("src/f{i}.rs"), "x", "function"))
                .chain(std::iter::once(ent("tests/t.rs", "t", "function")))
                .collect(),
            vec![],
        );
        let db = DuckDbBackend::open(&root).unwrap();
        let capped = db.explore_files_capped(None, 3).unwrap();
        let src_count = capped.iter().filter(|(d, _, _)| d == "src").count();
        let tests_count = capped.iter().filter(|(d, _, _)| d == "tests").count();
        assert_eq!(src_count, 3);
        assert_eq!(tests_count, 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_projects_returns_single_root() {
        let root = tmpdir("list_proj");
        seed(
            &root,
            vec![ent("a.rs", "x", "function")],
            vec![refr("b.rs", Some("c"), "x", "call", 1)],
        );
        let db = DuckDbBackend::open(&root).unwrap();
        assert_eq!(db.list_projects().unwrap(), vec![String::new()]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn get_file_symbols_parity_with_in_memory() {
        let root = tmpdir("file_parity");
        let mut entities = Vec::new();
        for i in 0..15 {
            let mut e = ent("src/core.rs", &format!("sym{i}"), "function");
            e.line_start = (i as u32) * 10 + 1;
            e.line_end = e.line_start + 5;
            entities.push(e);
        }
        entities.push(ent("src/other.rs", "other", "function"));
        seed(&root, entities, vec![]);

        let db = DuckDbBackend::open(&root).unwrap();
        let idx = crate::query::index::Index::load(&root).unwrap();

        let mut from_db = db.get_file_symbols("src/core.rs", None, 0).unwrap();
        let mut from_idx: Vec<Entity> = idx
            .get_file_symbols("src/core.rs", None, 0)
            .into_iter()
            .cloned()
            .collect();
        // Both backends should produce the same set; DuckDB already
        // sorts by line_start, so we sort the in-memory side to match.
        from_db.sort_by_key(|e| e.line_start);
        from_idx.sort_by_key(|e| e.line_start);

        // Compare the scalar columns the DuckDB backend populates.
        let project = |e: &Entity| (
            e.file.clone(),
            e.name.clone(),
            e.kind.clone(),
            e.line_start,
            e.line_end,
            e.parent.clone(),
        );
        let a: Vec<_> = from_db.iter().map(project).collect();
        let b: Vec<_> = from_idx.iter().map(project).collect();
        assert_eq!(a, b);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_jsonl_opens_as_empty_backend() {
        // Fresh `.sigil/` with no JSONL files should produce an empty
        // backend, not an error. Rationale: pre-index callers (e.g.
        // `sigil query` running before `sigil index` gets a chance)
        // should get structured "no results" rather than a hard failure.
        // The caller-facing staleness check (`Backend::load`) handles the
        // truly-unsafe case of an empty index.
        let root = tmpdir("empty");
        std::fs::create_dir_all(root.join(".sigil")).unwrap();
        let db = DuckDbBackend::open(&root).expect("open should succeed on empty .sigil/");
        assert_eq!(db.len().unwrap(), (0, 0));
        assert!(db.get_callers("anything", None, 0).unwrap().is_empty());
        assert!(db.get_file_symbols("missing.rs", None, 0).unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
