//! Per-day usage/log export to disk (JSONL logs + CSV summaries).
//!
//! Every request that finishes is recorded in the request-level usage view. To
//! make that history durable and inspectable outside the database, a background job
//! exports each UTC day's rows once the day has closed:
//!
//! - `<data_dir>/exports/usage-YYYY-MM-DD.jsonl` — one JSON object per request
//!   (append-friendly, lossless, streamable; the raw per-request record).
//! - `<data_dir>/exports/usage-YYYY-MM-DD.csv` — a flat per-request table for
//!   spreadsheets, plus `<data_dir>/exports/summary-YYYY-MM-DD.csv` with the
//!   per-day totals.
//!
//! Files older than the retention window may be pruned from the dashboard or by
//! deleting them locally; the DB rows are independent and unaffected.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Duration, Utc};

use crate::db::{self, Pool, UsageLogRow};

/// Export every closed day that is not yet on disk (up to `lookback_days`),
/// then prune files older than `retention_days`. Best-effort: any failure is
/// logged and never affects the data plane.
pub async fn run_export(
    pool: &Pool,
    dir: &Path,
    lookback_days: i64,
    retention_days: i64,
) -> Result<u32> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let today = Utc::now().date_naive();
    let mut written = 0u32;
    // Closed days: [today - lookback, today).
    for offset in 1..=lookback_days {
        let day = today - Duration::days(offset);
        let day_str = day.format("%Y-%m-%d").to_string();
        let path = dir.join(format!("usage-{day}.jsonl"));
        if path.exists() {
            continue;
        }
        let from = format!("{day}T00:00:00Z");
        // The next day's midnight bounds this day.
        let next = day + Duration::days(1);
        let to = format!("{next}T00:00:00Z");
        let rows = db::usage_between(pool, &from, &to).await?;
        if rows.is_empty() {
            continue;
        }
        write_day(dir, &day_str, &rows)?;
        written += 1;
    }
    prune(dir, retention_days)?;
    Ok(written)
}

/// Export one specific day (used by the manual export action). Returns the two
/// file paths written.
pub async fn export_day(pool: &Pool, dir: &Path, day: &str) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let from = format!("{day}T00:00:00Z");
    let next = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .with_context(|| format!("invalid day '{day}'"))?
        + Duration::days(1);
    let to = format!("{next}T00:00:00Z");
    let rows = db::usage_between(pool, &from, &to).await?;
    let (jsonl, csv) = write_day(dir, day, &rows)?;
    Ok((jsonl, csv))
}

fn write_day(dir: &Path, day: &str, rows: &[UsageLogRow]) -> Result<(PathBuf, PathBuf)> {
    let jsonl_path = dir.join(format!("usage-{day}.jsonl"));
    let csv_path = dir.join(format!("usage-{day}.csv"));

    // JSONL: one request per line, full record.
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&jsonl_path)?;
        for r in rows {
            let line = serde_json::to_string(r).context("serializing usage row")?;
            writeln!(f, "{line}")?;
        }
    }

    // CSV: flat per-request table.
    {
        let mut w = csv::Writer::from_path(&csv_path)?;
        w.write_record([
            "ts",
            "request_id",
            "key_name",
            "client_format",
            "requested_model",
            "effective_model",
            "route_name",
            "fallback_hops",
            "status",
            "status_code",
            "latency_ms",
            "ttft_ms",
            "input_tokens",
            "output_tokens",
            "cached_tokens",
            "cache_write_tokens",
            "thinking_tokens",
            "cost_usd",
            "cost_known",
            "usage_confidence",
            "cache_status",
            "serving_account",
            "serving_provider",
            "error_message",
        ])?;
        for r in rows {
            w.write_record([
                r.ts.clone(),
                r.request_id.clone(),
                r.key_name.clone().unwrap_or_default(),
                r.client_format.clone(),
                r.requested_model.clone(),
                r.effective_model.clone().unwrap_or_default(),
                r.route_name.clone().unwrap_or_default(),
                r.fallback_hops.to_string(),
                r.status.clone(),
                r.status_code.to_string(),
                r.latency_ms.map(|v| v.to_string()).unwrap_or_default(),
                r.ttft_ms.map(|v| v.to_string()).unwrap_or_default(),
                r.input_tokens.map(|v| v.to_string()).unwrap_or_default(),
                r.output_tokens.map(|v| v.to_string()).unwrap_or_default(),
                r.cached_tokens.map(|v| v.to_string()).unwrap_or_default(),
                r.cache_write_tokens
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                r.thinking_tokens.map(|v| v.to_string()).unwrap_or_default(),
                r.cost_usd.map(|v| format!("{v:.6}")).unwrap_or_default(),
                r.cost_known.to_string(),
                r.usage_confidence.clone(),
                r.cache_status.clone(),
                r.serving_account.clone().unwrap_or_default(),
                r.serving_provider.clone().unwrap_or_default(),
                r.error_message.clone().unwrap_or_default(),
            ])?;
        }
        w.flush()?;
    }

    // Per-day summary row.
    {
        let total_in: i64 = rows.iter().filter_map(|r| r.input_tokens).sum();
        let total_out: i64 = rows.iter().filter_map(|r| r.output_tokens).sum();
        let total_cached: i64 = rows.iter().filter_map(|r| r.cached_tokens).sum();
        let total_cache_write: i64 = rows.iter().filter_map(|r| r.cache_write_tokens).sum();
        let total_thinking: i64 = rows.iter().filter_map(|r| r.thinking_tokens).sum();
        let total_cost: f64 = rows.iter().filter_map(|r| r.cost_usd).sum();
        let fallbacks = rows.iter().filter(|r| r.fallback_hops > 0).count();
        let errors = rows
            .iter()
            .filter(|r| {
                r.status_code >= 500 || r.status == "upstream_error" || r.status == "stream_error"
            })
            .count();
        let summary_path = dir.join(format!("summary-{day}.csv"));
        let mut w = csv::Writer::from_path(&summary_path)?;
        w.write_record([
            "day",
            "requests",
            "input_tokens",
            "output_tokens",
            "cached_tokens",
            "cache_write_tokens",
            "thinking_tokens",
            "cost_usd",
            "fallbacks",
            "errors",
        ])?;
        w.write_record([
            day,
            &rows.len().to_string(),
            &total_in.to_string(),
            &total_out.to_string(),
            &total_cached.to_string(),
            &total_cache_write.to_string(),
            &total_thinking.to_string(),
            &format!("{total_cost:.6}"),
            &fallbacks.to_string(),
            &errors.to_string(),
        ])?;
        w.flush()?;
    }

    Ok((jsonl_path, csv_path))
}

fn export_file_day(name: &str) -> Option<chrono::NaiveDate> {
    let date = if let Some(rest) = name.strip_prefix("usage-") {
        rest.strip_suffix(".jsonl")
            .or_else(|| rest.strip_suffix(".csv"))?
    } else {
        let rest = name.strip_prefix("summary-")?;
        rest.strip_suffix(".csv")?
    };

    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// Remove export files older than `retention_days` (by filename date).
pub fn prune(dir: &Path, retention_days: i64) -> Result<u64> {
    if retention_days <= 0 {
        return Ok(0);
    }
    let cutoff = Utc::now().date_naive() - Duration::days(retention_days);
    prune_before(dir, cutoff)
}

fn prune_before(dir: &Path, cutoff: chrono::NaiveDate) -> Result<u64> {
    let mut removed = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(0);
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if export_file_day(&name).is_some_and(|day| day < cutoff)
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }

    Ok(removed)
}

/// A listing of export files for the dashboard, with size and day.
#[derive(serde::Serialize)]
pub struct ExportFile {
    pub name: String,
    pub day: String,
    pub kind: String,
    pub bytes: u64,
}

pub fn list_files(dir: &Path) -> Vec<ExportFile> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
        let (kind, day) = match name.split_once('-') {
            Some((k, rest)) => (
                k.to_string(),
                rest.split('.').next().unwrap_or("").to_string(),
            ),
            None => (name.clone(), String::new()),
        };
        out.push(ExportFile {
            name,
            day,
            kind,
            bytes,
        });
    }
    out.sort_by(|a, b| b.day.cmp(&a.day).then(a.name.cmp(&b.name)));
    out
}

/// Remove a single export file by name (basename only; path traversal is
/// rejected so the endpoint cannot delete arbitrary files).
pub fn delete_file(dir: &Path, name: &str) -> Result<bool> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        anyhow::bail!("invalid file name");
    }
    let path = dir.join(name);
    if !path.starts_with(dir) {
        anyhow::bail!("invalid file name");
    }
    match std::fs::remove_file(&path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("kinetix-export-test-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn parses_only_supported_export_file_names() {
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap();

        assert_eq!(export_file_day("usage-2026-08-31.jsonl"), Some(expected));
        assert_eq!(export_file_day("usage-2026-08-31.csv"), Some(expected));
        assert_eq!(export_file_day("summary-2026-08-31.csv"), Some(expected));

        assert_eq!(export_file_day("usage-2026-08-31.txt"), None);
        assert_eq!(export_file_day("summary-2026-08-31.jsonl"), None);
        assert_eq!(export_file_day("other-2026-08-31.csv"), None);
        assert_eq!(export_file_day("usage-not-a-date.csv"), None);
    }

    #[test]
    fn prune_before_removes_only_old_supported_exports() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let old = [
            "usage-2026-08-31.jsonl",
            "usage-2026-08-31.csv",
            "summary-2026-08-31.csv",
        ];
        let keep = [
            "usage-2026-09-01.jsonl",
            "usage-2026-09-02.csv",
            "usage-2026-08-31.txt",
            "usage-not-a-date.csv",
            "other-2026-08-31.csv",
            "README",
        ];

        for name in old.iter().chain(keep.iter()) {
            std::fs::write(dir.join(name), b"test").unwrap();
        }

        let cutoff = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let removed = prune_before(&dir, cutoff).unwrap();

        assert_eq!(removed, old.len() as u64);
        for name in old {
            assert!(!dir.join(name).exists(), "{name} should have been pruned");
        }
        for name in keep {
            assert!(dir.join(name).exists(), "{name} should have been retained");
        }

        std::fs::remove_dir_all(dir).unwrap();
    }
}
