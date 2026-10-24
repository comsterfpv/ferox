//! Plain-SQL dump of a single schema, generated from the catalogs — no `pg_dump`
//! binary needed (so it also works through ferox's own SSH tunnel and with any
//! server version ≥ 12 regardless of the locally installed client tools).
//!
//! Everything runs in one `REPEATABLE READ READ ONLY` transaction, so structure
//! and data come from a single consistent snapshot. Table data is streamed
//! through a server-side cursor in `FETCH_ROWS` batches and written straight to
//! disk — a table is never held in RAM, and the 50k-row cap of `execute_query`
//! doesn't apply. All queries use `simple_query` (text protocol), so values are
//! written back exactly as the server rendered them (no `parse_text_cell`
//! round-trip that could turn `007` into `7`).
//!
//! Output order mirrors what a restore needs: extensions → schema → enum/domain
//! types → sequences → functions (that don't take/return a table row type) →
//! tables + CHECKs → remaining functions → data → setval → PK/UNIQUE/EXCLUDE →
//! indexes → foreign keys → views/matviews → triggers. Foreign keys, unique
//! constraints and triggers come after the data so load order between tables
//! doesn't matter and triggers don't fire during restore.
//!
//! Deliberately not included (same as `pg_dump --no-owner --no-privileges`
//! plus a few rarer objects): owners, GRANTs, RLS policies, comments, composite
//! and range types, aggregates/operators, classic (non-partition) inheritance.

use std::io::Write;

use anyhow::{bail, Result};
use tokio_postgres::{Client, SimpleQueryMessage};

use crate::db::query::{quote_ident, quote_literal, INSERT_BATCH_ROWS};

/// Rows pulled per `FETCH` from the data cursor.
const FETCH_ROWS: usize = INSERT_BATCH_ROWS * 10;

pub struct DumpStats {
    pub tables: usize,
    pub rows: u64,
}

type Row = Vec<Option<String>>;

/// Run `sql` via `simple_query` and return every row as raw text cells.
async fn query_rows(client: &Client, sql: &str) -> Result<Vec<Row>> {
    let msgs = client.simple_query(sql).await?;
    Ok(msgs
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some((0..r.len()).map(|i| r.get(i).map(str::to_owned)).collect()),
            _ => None,
        })
        .collect())
}

fn cell(row: &Row, i: usize) -> &str {
    row.get(i).and_then(|c| c.as_deref()).unwrap_or("")
}

/// SQL fragment: object `oid_expr` is not a member of an extension.
fn not_ext(oid_expr: &str) -> String {
    format!("NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = {oid_expr} AND d.deptype = 'e')")
}

struct Column {
    name: String,
    ty: String,
    not_null: bool,
    default: Option<String>,
    /// `a` = GENERATED ALWAYS AS IDENTITY, `d` = BY DEFAULT, empty = none.
    identity: String,
    /// `s` = stored generated, `v` = virtual (PG 18), empty = regular column.
    generated: String,
    collation: Option<String>,
}

struct Table {
    oid: String,
    name: String,
    /// `r` = plain table, `p` = partitioned parent.
    kind: String,
    unlogged: bool,
    partition_key: Option<String>,
    /// For partitions: `(qualified parent, partition bound)`.
    partition_of: Option<(String, String)>,
    columns: Vec<Column>,
}

/// Dump `schema` to `path`. Writes to `<path>.partial` first and renames on
/// success, so a failed or cancelled dump never leaves a truncated file behind
/// under the name the user chose.
pub async fn dump_schema(client: &Client, schema: &str, path: &str, with_data: bool) -> Result<DumpStats> {
    let version: i32 = query_rows(client, "SELECT current_setting('server_version_num')")
        .await?
        .first()
        .map(|r| cell(r, 0).parse().unwrap_or(0))
        .unwrap_or(0);
    if version < 120000 {
        bail!("Schema dump requires PostgreSQL 12 or newer (server_version_num = {version})");
    }

    let tmp_path = format!("{path}.partial");
    client
        .simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await?;
    let result = async {
        // pg_catalog-only search_path makes every pg_get_*def / format_type
        // output fully schema-qualified; fixed date/float styles make the text
        // form of values unambiguous and lossless on restore (as pg_dump does).
        client
            .simple_query(
                "SET LOCAL search_path = pg_catalog; SET LOCAL datestyle = 'ISO'; \
                 SET LOCAL intervalstyle = 'postgres'; SET LOCAL extra_float_digits = 3",
            )
            .await?;
        let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp_path)?);
        let stats = write_dump(client, &mut out, schema, with_data, version).await?;
        out.flush()?;
        Ok::<_, anyhow::Error>(stats)
    }
    .await;
    // Read-only transaction — nothing to keep either way.
    let _ = client.simple_query("ROLLBACK").await;

    match result {
        Ok(stats) => {
            std::fs::rename(&tmp_path, path)?;
            Ok(stats)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

async fn write_dump(
    client: &Client,
    out: &mut impl Write,
    schema: &str,
    with_data: bool,
    version: i32,
) -> Result<DumpStats> {
    let s_lit = quote_literal(schema);
    let s_q = quote_ident(schema);
    let qualify = |name: &str| format!("{s_q}.{}", quote_ident(name));

    // ── Header ────────────────────────────────────────────────────────────────
    let info = query_rows(client, "SELECT current_database(), version()").await?;
    let (db, server) = info.first().map(|r| (cell(r, 0).to_owned(), cell(r, 1).to_owned())).unwrap_or_default();
    writeln!(out, "-- ferox schema dump")?;
    writeln!(out, "-- Schema:   {schema}")?;
    writeln!(out, "-- Database: {db}")?;
    writeln!(out, "-- Server:   {server}")?;
    writeln!(out, "-- Created:  {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))?;
    writeln!(out, "-- Contents: {}", if with_data { "structure + data" } else { "structure only" })?;
    writeln!(out, "-- Not included: owners, privileges, RLS policies, comments, composite/range types,")?;
    writeln!(out, "-- aggregates/operators, non-partition table inheritance.")?;
    writeln!(out)?;
    writeln!(out, "SET client_encoding = 'UTF8';")?;
    writeln!(out, "SET standard_conforming_strings = on;")?;
    writeln!(out, "SET check_function_bodies = false;")?;
    writeln!(out, "SET client_min_messages = warning;")?;
    writeln!(out)?;
    writeln!(out, "CREATE SCHEMA IF NOT EXISTS {s_q};")?;
    writeln!(out, "SET search_path = {s_q}, public;")?;
    writeln!(out)?;

    // ── Extensions (database-wide; a table may depend on any of them) ────────
    let exts = query_rows(
        client,
        "SELECT e.extname, n.nspname FROM pg_extension e \
         JOIN pg_namespace n ON n.oid = e.extnamespace \
         WHERE e.extname <> 'plpgsql' ORDER BY e.extname",
    )
    .await?;
    if !exts.is_empty() {
        writeln!(out, "-- Extensions")?;
        for r in &exts {
            writeln!(
                out,
                "CREATE EXTENSION IF NOT EXISTS {} WITH SCHEMA {};",
                quote_ident(cell(r, 0)),
                quote_ident(cell(r, 1))
            )?;
        }
        writeln!(out)?;
    }

    // ── Enum and domain types ────────────────────────────────────────────────
    let enums = query_rows(
        client,
        &format!(
            "SELECT t.typname, string_agg(quote_literal(e.enumlabel), ', ' ORDER BY e.enumsortorder) \
             FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
             JOIN pg_enum e ON e.enumtypid = t.oid \
             WHERE n.nspname = {s_lit} AND {} \
             GROUP BY t.typname ORDER BY t.typname",
            not_ext("t.oid")
        ),
    )
    .await?;
    for r in &enums {
        writeln!(out, "CREATE TYPE {} AS ENUM ({});", qualify(cell(r, 0)), cell(r, 1))?;
    }
    let domains = query_rows(
        client,
        &format!(
            "SELECT t.typname, format_type(t.typbasetype, t.typtypmod), t.typnotnull, t.typdefault, \
                    (SELECT string_agg(' CONSTRAINT ' || quote_ident(c.conname) || ' ' || pg_get_constraintdef(c.oid), '' ORDER BY c.conname) \
                     FROM pg_constraint c WHERE c.contypid = t.oid AND c.contype = 'c') \
             FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE n.nspname = {s_lit} AND t.typtype = 'd' AND {} ORDER BY t.typname",
            not_ext("t.oid")
        ),
    )
    .await?;
    for r in &domains {
        let mut def = format!("CREATE DOMAIN {} AS {}", qualify(cell(r, 0)), cell(r, 1));
        if let Some(d) = r.get(3).and_then(|c| c.as_deref()) {
            def.push_str(&format!(" DEFAULT {d}"));
        }
        if cell(r, 2) == "t" {
            def.push_str(" NOT NULL");
        }
        def.push_str(cell(r, 4));
        writeln!(out, "{def};")?;
    }
    if !enums.is_empty() || !domains.is_empty() {
        writeln!(out)?;
    }

    // ── Sequences (identity sequences are created by their column) ───────────
    let seqs = query_rows(
        client,
        &format!(
            "SELECT c.relname, format_type(s.seqtypid, NULL), s.seqincrement, s.seqmin, s.seqmax, \
                    s.seqstart, s.seqcache, s.seqcycle \
             FROM pg_sequence s JOIN pg_class c ON c.oid = s.seqrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {s_lit} \
               AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = c.oid \
                               AND d.classid = 'pg_class'::regclass AND d.deptype IN ('i', 'e')) \
             ORDER BY c.relname"
        ),
    )
    .await?;
    for r in &seqs {
        writeln!(
            out,
            "CREATE SEQUENCE {} AS {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {} CACHE {}{};",
            qualify(cell(r, 0)),
            cell(r, 1),
            cell(r, 2),
            cell(r, 3),
            cell(r, 4),
            cell(r, 5),
            cell(r, 6),
            if cell(r, 7) == "t" { " CYCLE" } else { " NO CYCLE" }
        )?;
    }
    if !seqs.is_empty() {
        writeln!(out)?;
    }

    // ── Functions / procedures ───────────────────────────────────────────────
    // Ones whose signature mentions a table row type can only be created after
    // the tables; everything else goes first so column defaults, generated
    // columns and CHECKs can call them.
    let funcs = query_rows(
        client,
        &format!(
            "SELECT pg_get_functiondef(p.oid), \
                    EXISTS (SELECT 1 FROM pg_type t WHERE t.typrelid <> 0 AND (t.oid = p.prorettype \
                            OR t.oid = ANY(p.proargtypes::oid[]) \
                            OR t.oid = ANY(coalesce(p.proallargtypes, '{{}}'::oid[])))) \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = {s_lit} AND p.prokind IN ('f', 'p') AND {} \
             ORDER BY p.proname, p.oid",
            not_ext("p.oid")
        ),
    )
    .await?;
    let (late_funcs, early_funcs): (Vec<&Row>, Vec<&Row>) = funcs.iter().partition(|r| cell(r, 1) == "t");
    write_functions(out, &early_funcs)?;

    // ── Tables ───────────────────────────────────────────────────────────────
    let mut tables = load_tables(client, &s_lit).await?;
    load_columns(client, &s_lit, &mut tables).await?;
    let constraints = query_rows(
        client,
        &format!(
            "SELECT c.conrelid, r.relname, c.conname, pg_get_constraintdef(c.oid), c.contype \
             FROM pg_constraint c JOIN pg_class r ON r.oid = c.conrelid \
             JOIN pg_namespace n ON n.oid = r.relnamespace \
             WHERE n.nspname = {s_lit} AND r.relkind IN ('r', 'p') AND c.contype IN ('c', 'p', 'u', 'x', 'f') \
               AND c.conislocal AND c.conparentid = 0 AND {} \
             ORDER BY r.relname, c.conname",
            not_ext("r.oid")
        ),
    )
    .await?;
    let add_constraints = |out: &mut dyn Write, types: &[&str]| -> Result<()> {
        for r in constraints.iter().filter(|r| types.contains(&cell(r, 4))) {
            writeln!(
                out,
                "ALTER TABLE {} ADD CONSTRAINT {} {};",
                qualify(cell(r, 1)),
                quote_ident(cell(r, 2)),
                cell(r, 3)
            )?;
        }
        Ok(())
    };

    for t in &tables {
        writeln!(out, "{}", create_table_sql(t, &qualify))?;
        writeln!(out)?;
    }
    add_constraints(out, &["c"])?;
    let owned = query_rows(
        client,
        &format!(
            "SELECT seq.relname, ps.last_value, tn.nspname, tbl.relname, att.attname, dep.deptype \
             FROM pg_class seq JOIN pg_namespace n ON n.oid = seq.relnamespace \
             JOIN pg_sequences ps ON ps.schemaname = n.nspname AND ps.sequencename = seq.relname \
             LEFT JOIN pg_depend dep ON dep.objid = seq.oid AND dep.classid = 'pg_class'::regclass \
                  AND dep.refclassid = 'pg_class'::regclass AND dep.deptype IN ('a', 'i') \
             LEFT JOIN pg_class tbl ON tbl.oid = dep.refobjid \
             LEFT JOIN pg_namespace tn ON tn.oid = tbl.relnamespace \
             LEFT JOIN pg_attribute att ON att.attrelid = dep.refobjid AND att.attnum = dep.refobjsubid \
             WHERE n.nspname = {s_lit} AND seq.relkind = 'S' ORDER BY seq.relname"
        ),
    )
    .await?;
    for r in owned.iter().filter(|r| cell(r, 5) == "a") {
        writeln!(
            out,
            "ALTER SEQUENCE {} OWNED BY {}.{}.{};",
            qualify(cell(r, 0)),
            quote_ident(cell(r, 2)),
            quote_ident(cell(r, 3)),
            quote_ident(cell(r, 4))
        )?;
    }
    writeln!(out)?;
    write_functions(out, &late_funcs)?;

    // ── Data ─────────────────────────────────────────────────────────────────
    let mut total_rows = 0u64;
    if with_data {
        for t in tables.iter().filter(|t| t.kind == "r") {
            total_rows += write_table_data(client, out, t, &qualify(&t.name)).await?;
        }
        // Sequence positions — after the data so restored rows don't collide
        // with the next generated value.
        for r in owned.iter().filter(|r| r.get(1).is_some_and(|v| v.is_some())) {
            let target = if cell(r, 5) == "i" {
                format!(
                    "pg_catalog.pg_get_serial_sequence({}, {})",
                    quote_literal(&format!("{}.{}", quote_ident(cell(r, 2)), quote_ident(cell(r, 3)))),
                    quote_literal(cell(r, 4))
                )
            } else {
                quote_literal(&qualify(cell(r, 0)))
            };
            writeln!(out, "SELECT pg_catalog.setval({target}, {}, true);", cell(r, 1))?;
        }
        writeln!(out)?;
    }

    // ── Keys, indexes, foreign keys ──────────────────────────────────────────
    add_constraints(out, &["p", "u", "x"])?;
    write_indexes(client, out, &s_lit, &["r", "p"]).await?;
    add_constraints(out, &["f"])?;
    writeln!(out)?;

    // ── Views and materialized views (oid order ≈ creation/dependency order) ─
    let views = query_rows(
        client,
        &format!(
            "SELECT c.relname, c.relkind, pg_get_viewdef(c.oid, true), c.relispopulated \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {s_lit} AND c.relkind IN ('v', 'm') AND {} ORDER BY c.oid",
            not_ext("c.oid")
        ),
    )
    .await?;
    for r in &views {
        let def = cell(r, 2).trim().trim_end_matches(';');
        if cell(r, 1) == "m" {
            let populate = if with_data && cell(r, 3) == "t" { "WITH DATA" } else { "WITH NO DATA" };
            writeln!(out, "CREATE MATERIALIZED VIEW {} AS\n{def}\n{populate};\n", qualify(cell(r, 0)))?;
        } else {
            writeln!(out, "CREATE VIEW {} AS\n{def};\n", qualify(cell(r, 0)))?;
        }
    }
    write_indexes(client, out, &s_lit, &["m"]).await?;

    // ── Triggers (last, so restoring the data never fires them) ──────────────
    // Partition clones of a parent's trigger are recreated by the parent's own
    // CREATE TRIGGER: internal before PG 13, marked by tgparentid since.
    let clone_filter = if version >= 130000 { "AND t.tgparentid = 0" } else { "" };
    let triggers = query_rows(
        client,
        &format!(
            "SELECT pg_get_triggerdef(t.oid, false) \
             FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {s_lit} AND NOT t.tgisinternal {clone_filter} \
             ORDER BY c.relname, t.tgname"
        ),
    )
    .await?;
    for r in &triggers {
        writeln!(out, "{};", cell(r, 0))?;
    }

    Ok(DumpStats { tables: tables.len(), rows: total_rows })
}

fn write_functions(out: &mut impl Write, funcs: &[&Row]) -> Result<()> {
    for r in funcs {
        writeln!(out, "{};\n", cell(r, 0).trim_end())?;
    }
    Ok(())
}

async fn load_tables(client: &Client, s_lit: &str) -> Result<Vec<Table>> {
    // Partitions after plain/parent tables; oid order puts a parent before its children.
    let rows = query_rows(
        client,
        &format!(
            "SELECT c.oid, c.relname, c.relkind, c.relpersistence, \
                    CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END, \
                    (SELECT quote_ident(pn.nspname) || '.' || quote_ident(pc.relname) \
                     FROM pg_inherits i JOIN pg_class pc ON pc.oid = i.inhparent \
                     JOIN pg_namespace pn ON pn.oid = pc.relnamespace \
                     WHERE c.relispartition AND i.inhrelid = c.oid), \
                    CASE WHEN c.relispartition THEN pg_get_expr(c.relpartbound, c.oid) END \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {s_lit} AND c.relkind IN ('r', 'p') AND {} \
             ORDER BY c.relispartition, c.oid",
            not_ext("c.oid")
        ),
    )
    .await?;
    Ok(rows
        .iter()
        .map(|r| Table {
            oid: cell(r, 0).to_owned(),
            name: cell(r, 1).to_owned(),
            kind: cell(r, 2).to_owned(),
            unlogged: cell(r, 3) == "u",
            partition_key: r.get(4).cloned().flatten(),
            partition_of: match (r.get(5).cloned().flatten(), r.get(6).cloned().flatten()) {
                (Some(parent), Some(bound)) => Some((parent, bound)),
                _ => None,
            },
            columns: Vec::new(),
        })
        .collect())
}

async fn load_columns(client: &Client, s_lit: &str, tables: &mut [Table]) -> Result<()> {
    let rows = query_rows(
        client,
        &format!(
            "SELECT a.attrelid, a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                    pg_get_expr(ad.adbin, ad.adrelid), a.attidentity, a.attgenerated, \
                    CASE WHEN a.attcollation <> t.typcollation THEN \
                        (SELECT quote_ident(cn.nspname) || '.' || quote_ident(co.collname) \
                         FROM pg_collation co JOIN pg_namespace cn ON cn.oid = co.collnamespace \
                         WHERE co.oid = a.attcollation) END \
             FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_type t ON t.oid = a.atttypid \
             LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
             WHERE n.nspname = {s_lit} AND c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attrelid, a.attnum"
        ),
    )
    .await?;
    for r in &rows {
        if let Some(t) = tables.iter_mut().find(|t| t.oid == cell(r, 0)) {
            t.columns.push(Column {
                name: cell(r, 1).to_owned(),
                ty: cell(r, 2).to_owned(),
                not_null: cell(r, 3) == "t",
                default: r.get(4).cloned().flatten(),
                identity: cell(r, 5).to_owned(),
                generated: cell(r, 6).to_owned(),
                collation: r.get(7).cloned().flatten(),
            });
        }
    }
    Ok(())
}

fn create_table_sql(t: &Table, qualify: &dyn Fn(&str) -> String) -> String {
    let unlogged = if t.unlogged { "UNLOGGED " } else { "" };
    // A partition's columns, defaults and NOT NULLs come from its parent.
    if let Some((parent, bound)) = &t.partition_of {
        let mut sql = format!("CREATE {unlogged}TABLE {} PARTITION OF {parent} {bound}", qualify(&t.name));
        if let Some(key) = &t.partition_key {
            sql.push_str(&format!(" PARTITION BY {key}"));
        }
        sql.push(';');
        return sql;
    }
    let cols = t
        .columns
        .iter()
        .map(|c| {
            let mut def = format!("    {} {}", quote_ident(&c.name), c.ty);
            if let Some(coll) = &c.collation {
                def.push_str(&format!(" COLLATE {coll}"));
            }
            match (c.generated.as_str(), c.identity.as_str(), &c.default) {
                ("s", _, Some(expr)) => def.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED")),
                ("v", _, Some(expr)) => def.push_str(&format!(" GENERATED ALWAYS AS ({expr}) VIRTUAL")),
                (_, "a", _) => def.push_str(" GENERATED ALWAYS AS IDENTITY"),
                (_, "d", _) => def.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                (_, _, Some(expr)) => def.push_str(&format!(" DEFAULT {expr}")),
                _ => {}
            }
            if c.not_null {
                def.push_str(" NOT NULL");
            }
            def
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let mut sql = format!("CREATE {unlogged}TABLE {} (\n{cols}\n)", qualify(&t.name));
    if let Some(key) = &t.partition_key {
        sql.push_str(&format!(" PARTITION BY {key}"));
    }
    sql.push(';');
    sql
}

/// Stream one table's rows into batched INSERT statements. Returns rows written.
async fn write_table_data(client: &Client, out: &mut impl Write, t: &Table, table_q: &str) -> Result<u64> {
    // Generated columns can't be inserted into; GENERATED ALWAYS identity
    // columns need OVERRIDING SYSTEM VALUE to keep their original values.
    let cols: Vec<&Column> = t.columns.iter().filter(|c| c.generated.is_empty()).collect();
    if cols.is_empty() {
        return Ok(0);
    }
    let col_list = cols.iter().map(|c| quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
    let overriding = if cols.iter().any(|c| c.identity == "a") { " OVERRIDING SYSTEM VALUE" } else { "" };

    client
        .simple_query(&format!(
            "DECLARE ferox_dump_cur NO SCROLL CURSOR FOR SELECT {col_list} FROM ONLY {table_q}"
        ))
        .await?;
    let fetch = format!("FETCH {FETCH_ROWS} FROM ferox_dump_cur");
    let mut written = 0u64;
    loop {
        let rows = query_rows(client, &fetch).await?;
        if rows.is_empty() {
            break;
        }
        if written == 0 {
            writeln!(out, "-- Data for {table_q}")?;
        }
        for chunk in rows.chunks(INSERT_BATCH_ROWS) {
            writeln!(out, "INSERT INTO {table_q} ({col_list}){overriding} VALUES")?;
            for (i, row) in chunk.iter().enumerate() {
                let vals = row
                    .iter()
                    .map(|v| v.as_deref().map_or_else(|| "NULL".to_owned(), quote_literal))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(out, "    ({vals}){}", if i + 1 == chunk.len() { ";" } else { "," })?;
            }
        }
        written += rows.len() as u64;
    }
    client.simple_query("CLOSE ferox_dump_cur").await?;
    if written > 0 {
        writeln!(out)?;
    }
    Ok(written)
}

async fn write_indexes(client: &Client, out: &mut impl Write, s_lit: &str, relkinds: &[&str]) -> Result<()> {
    let kinds = relkinds.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", ");
    // Skip indexes that back a constraint (created by ADD CONSTRAINT) and
    // partition children of a partitioned index (created by the parent's index).
    let rows = query_rows(
        client,
        &format!(
            "SELECT pg_get_indexdef(i.indexrelid), c.relkind \
             FROM pg_index i JOIN pg_class c ON c.oid = i.indrelid \
             JOIN pg_class ic ON ic.oid = i.indexrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {s_lit} AND c.relkind IN ({kinds}) AND {} \
               AND NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = i.indexrelid \
                               AND k.contype IN ('p', 'u', 'x')) \
               AND NOT EXISTS (SELECT 1 FROM pg_inherits h WHERE h.inhrelid = i.indexrelid) \
             ORDER BY c.relname, ic.relname",
            not_ext("c.oid")
        ),
    )
    .await?;
    for r in &rows {
        let def = cell(r, 0);
        // pg_get_indexdef says `ON ONLY` for a partitioned table, which would
        // create the index on the parent alone; without ONLY it cascades to
        // every partition, matching the source.
        let def = if cell(r, 1) == "p" { def.replacen(" ON ONLY ", " ON ", 1) } else { def.to_owned() };
        writeln!(out, "{def};")?;
    }
    if !rows.is_empty() {
        writeln!(out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConnectionProfile, SslMode};

    fn test_profile() -> ConnectionProfile {
        ConnectionProfile {
            name: "ferox-test".into(),
            host: std::env::var("FEROX_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
            port: std::env::var("FEROX_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432),
            user: std::env::var("FEROX_TEST_PG_USER").unwrap_or_else(|_| "postgres".into()),
            password: std::env::var("FEROX_TEST_PG_PASSWORD").unwrap_or_else(|_| "postgres".into()),
            database: std::env::var("FEROX_TEST_PG_DATABASE").unwrap_or_else(|_| "postgres".into()),
            ssl: SslMode::Disable,
            ssh_tunnel: None,
            group: None,
            color: None,
        }
    }

    async fn connect() -> Client {
        let (client, conn) = tokio_postgres::connect(&test_profile().connection_string(), tokio_postgres::NoTls)
            .await
            .expect("connect failed — is Postgres reachable?");
        tokio::spawn(conn);
        client
    }

    async fn object_counts(client: &Client, schema: &str) -> Row {
        let s = quote_literal(schema);
        query_rows(
            client,
            &format!(
                "SELECT (SELECT count(*) FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace WHERE n.nspname = {s}), \
                        (SELECT count(*) FROM pg_indexes WHERE schemaname = {s}), \
                        (SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {s} AND NOT t.tgisinternal)"
            ),
        )
        .await
        .unwrap()
        .remove(0)
    }

    /// Dump a schema that exercises every object kind, restore it under another
    /// schema name, and compare data + structure. Needs a live Postgres, so
    /// `#[ignore]`d — run with
    /// `cargo test --bin ferox db::dump::tests -- --ignored`
    /// (env vars as in `db::connection::tests`).
    #[tokio::test]
    #[ignore]
    async fn dump_and_restore_roundtrip() {
        let client = connect().await;
        // Mixed case + space forces the server to always quote the name, so the
        // restore below can retarget the dump with a plain text replace.
        let src = "Ferox Dump Src";
        let dst = "ferox_dump_dst";
        let (src_q, dst_q) = (quote_ident(src), quote_ident(dst));
        client
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {src_q} CASCADE; DROP SCHEMA IF EXISTS {dst_q} CASCADE;
                 CREATE SCHEMA {src_q};
                 CREATE TYPE {src_q}.mood AS ENUM ('sad', 'ok', 'happy');
                 CREATE DOMAIN {src_q}.posint AS integer NOT NULL CHECK (VALUE > 0);
                 CREATE FUNCTION {src_q}.double_it(x int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT x * 2';
                 CREATE TABLE {src_q}.parent (
                     id serial PRIMARY KEY,
                     code text UNIQUE,
                     mood {src_q}.mood DEFAULT 'ok',
                     qty {src_q}.posint,
                     twice int GENERATED ALWAYS AS ({src_q}.double_it(qty)) STORED,
                     note text CHECK (note <> 'forbidden')
                 );
                 CREATE TABLE {src_q}.child (
                     id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                     parent_id int REFERENCES {src_q}.parent(id),
                     payload jsonb,
                     raw bytea,
                     at timestamptz,
                     tags text[]
                 );
                 CREATE INDEX child_payload_idx ON {src_q}.child USING btree ((payload->>'k'));
                 CREATE TABLE {src_q}.events (id int, day date) PARTITION BY RANGE (day);
                 CREATE TABLE {src_q}.events_2026 PARTITION OF {src_q}.events FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
                 CREATE INDEX events_day_idx ON {src_q}.events (day);
                 CREATE FUNCTION {src_q}.parent_code(p {src_q}.parent) RETURNS text LANGUAGE sql AS 'SELECT p.code';
                 CREATE VIEW {src_q}.v_parent AS SELECT id, code FROM {src_q}.parent WHERE qty > 1;
                 CREATE MATERIALIZED VIEW {src_q}.mv_count AS SELECT count(*) AS n FROM {src_q}.child;
                 CREATE FUNCTION {src_q}.touch() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
                 CREATE TRIGGER child_touch BEFORE UPDATE ON {src_q}.child FOR EACH ROW EXECUTE FUNCTION {src_q}.touch();
                 INSERT INTO {src_q}.parent (code, mood, qty, note) VALUES
                     ('a', 'happy', 1, 'it''s fine'), ('b', NULL, 2, E'line1\\nline2 \\\\ back'), ('c', 'sad', 3, NULL);
                 INSERT INTO {src_q}.child (parent_id, payload, raw, at, tags) VALUES
                     (1, '{{\"k\": \"v\"}}', '\\xdeadbeef', '2026-05-01 12:34:56.789+00', ARRAY['x', 'y, z']),
                     (2, NULL, NULL, NULL, NULL);
                 INSERT INTO {src_q}.events VALUES (1, '2026-03-04');
                 INSERT INTO {src_q}.parent (code, qty) SELECT 'g' || g, g FROM generate_series(10, 2600) g;
                 REFRESH MATERIALIZED VIEW {src_q}.mv_count;"
            ))
            .await
            .unwrap();

        let path = std::env::temp_dir().join(format!("ferox_dump_test_{}.sql", std::process::id()));
        let path_s = path.to_str().unwrap();
        let stats = dump_schema(&client, src, path_s, true).await.expect("dump failed");
        assert_eq!(stats.tables, 4); // parent, child, events, events_2026
        // parent: 3 + 2591 (spans several FETCH/INSERT batches), child: 2, events_2026: 1
        assert_eq!(stats.rows, 3 + 2591 + 2 + 1);
        assert!(!std::path::Path::new(&format!("{path_s}.partial")).exists());

        let sql = std::fs::read_to_string(&path).unwrap();
        client
            .batch_execute(&sql.replace(&src_q, &dst_q))
            .await
            .expect("restore failed");

        for (table, order) in [("parent", "id"), ("child", "id"), ("events", "id"), ("v_parent", "id"), ("mv_count", "n")] {
            let q = |s: &str| format!("SELECT t::text FROM {s}.{table} t ORDER BY {order}");
            let a = query_rows(&client, &q(&src_q)).await.unwrap();
            let b = query_rows(&client, &q(&dst_q)).await.unwrap();
            assert_eq!(a, b, "data mismatch in {table}");
        }
        // Sequences continue after the restored rows (serial and identity).
        let next = query_rows(&client, &format!("INSERT INTO {dst_q}.parent (code, qty) VALUES ('new', 5) RETURNING id"))
            .await
            .unwrap();
        assert_eq!(cell(&next[0], 0), "2595");
        let next = query_rows(&client, &format!("INSERT INTO {dst_q}.child (parent_id) VALUES (1) RETURNING id"))
            .await
            .unwrap();
        assert_eq!(cell(&next[0], 0), "3");
        // Constraints, indexes and triggers came along.
        assert_eq!(object_counts(&client, src).await, object_counts(&client, dst).await);
        assert!(
            client.batch_execute(&format!("INSERT INTO {dst_q}.child (parent_id) VALUES (-1)")).await.is_err(),
            "FK not restored"
        );

        // Structure-only dump has no INSERTs.
        let stats = dump_schema(&client, src, path_s, false).await.unwrap();
        assert_eq!(stats.rows, 0);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("INSERT INTO"));

        client
            .batch_execute(&format!("DROP SCHEMA {src_q} CASCADE; DROP SCHEMA {dst_q} CASCADE;"))
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
