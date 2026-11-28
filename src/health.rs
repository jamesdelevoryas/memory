//! `GET /health`: an unauthenticated summary for an external monitor. It
//! reports whether the database answers, how old the last successful backup
//! and the last consolidation run are (each writes a timestamp file), and how
//! full the disk is. 200 when all is well, 503 with the problems otherwise. It
//! reveals no note content.

use std::{path::PathBuf, sync::Arc};

use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;

use crate::store::Store;

#[derive(Clone)]
pub struct Health(Arc<Config>);

pub struct Config {
    pub store: Store,
    /// Directory whose filesystem is checked for free space (the database's).
    pub data_dir: PathBuf,
    pub max_disk_percent: f64,
    /// File holding the RFC 3339 time of the last successful backup; no
    /// backup check when unset.
    pub backup_stamp: Option<PathBuf>,
    pub max_backup_age_hours: f64,
    /// File holding the RFC 3339 time the nightly consolidator last finished;
    /// no consolidation check when unset.
    pub consolidation_stamp: Option<PathBuf>,
    pub max_consolidation_age_hours: f64,
}

impl Health {
    pub fn new(config: Config) -> Self {
        Self(Arc::new(config))
    }
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Report {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_age_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consolidation_age_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_used_percent: Option<f64>,
    pub problems: Vec<String>,
}

pub async fn handler(State(Health(c)): State<Health>) -> (StatusCode, Json<Report>) {
    let notes = c
        .store
        .count()
        .await
        .map_err(|e| format!("database: {e:#}"));
    let disk = disk_used_percent(&c.data_dir).map_err(|e| format!("disk: {e}"));
    let backup = c.backup_stamp.as_deref().map(|p| stamp_age("backup", p));
    let consolidation = c
        .consolidation_stamp
        .as_deref()
        .map(|p| stamp_age("consolidation", p));
    let mut report = evaluate(notes, disk, backup, &c);
    report.consolidation_age_hours = check_age(
        consolidation,
        "consolidation",
        c.max_consolidation_age_hours,
        &mut report.problems,
    );
    report.ok = report.problems.is_empty();
    let status = if report.ok {
        StatusCode::OK
    } else {
        tracing::warn!(problems = ?report.problems, "health check failing");
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(report))
}

/// Hours since the time a stamp file holds.
fn stamp_age(what: &str, path: &std::path::Path) -> Result<f64, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("{what}: can't read {}: {e}", path.display()))?;
    let at = chrono::DateTime::parse_from_rfc3339(text.trim())
        .map_err(|e| format!("{what}: bad timestamp in {}: {e}", path.display()))?;
    Ok((chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_minutes() as f64 / 60.0)
}

/// Records a problem when a stamp is missing or older than `max_hours`, and
/// returns its age rounded to a tenth of an hour.
fn check_age(
    age: Option<Result<f64, String>>,
    what: &str,
    max_hours: f64,
    problems: &mut Vec<String>,
) -> Option<f64> {
    match age? {
        Ok(age) => {
            if age > max_hours {
                problems.push(format!(
                    "last {what} was {age:.1} hours ago (limit {max_hours:.0})"
                ));
            }
            Some((age * 10.0).round() / 10.0)
        }
        Err(e) => {
            problems.push(e);
            None
        }
    }
}

fn evaluate(
    notes: Result<u64, String>,
    disk: Result<f64, String>,
    backup: Option<Result<f64, String>>,
    c: &Config,
) -> Report {
    let mut problems = Vec::new();
    let notes = notes.map_err(|e| problems.push(e)).ok();
    let disk = disk.map_err(|e| problems.push(e)).ok();
    if let Some(used) = disk
        && used > c.max_disk_percent
    {
        problems.push(format!(
            "disk is {used:.0}% full (limit {:.0}%)",
            c.max_disk_percent
        ));
    }
    let backup_age_hours = check_age(backup, "backup", c.max_backup_age_hours, &mut problems);
    Report {
        ok: problems.is_empty(),
        notes,
        backup_age_hours,
        consolidation_age_hours: None,
        disk_used_percent: disk.map(|d| d.round()),
        problems,
    }
}

fn disk_used_percent(dir: &std::path::Path) -> std::io::Result<f64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid NUL-terminated string and `stat` is a valid out-pointer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let total = stat.f_blocks as f64;
    let available = stat.f_bavail as f64;
    if total == 0.0 {
        return Ok(0.0);
    }
    Ok((total - available) / total * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn config() -> Config {
        let dir = std::env::temp_dir().join(format!("memory-health-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Config {
            store: Store::open(dir.join("t.db").to_str().unwrap())
                .await
                .unwrap(),
            data_dir: dir,
            max_disk_percent: 85.0,
            backup_stamp: None,
            max_backup_age_hours: 26.0,
            consolidation_stamp: None,
            max_consolidation_age_hours: 30.0,
        }
    }

    #[tokio::test]
    async fn healthy_and_unhealthy_reports() {
        let c = config().await;
        let ok = evaluate(Ok(3), Ok(40.2), Some(Ok(5.04)), &c);
        assert_eq!(
            ok,
            Report {
                ok: true,
                notes: Some(3),
                backup_age_hours: Some(5.0),
                consolidation_age_hours: None,
                disk_used_percent: Some(40.0),
                problems: vec![]
            }
        );
        let bad = evaluate(Err("database: down".into()), Ok(91.0), Some(Ok(30.0)), &c);
        assert!(!bad.ok);
        assert_eq!(bad.problems.len(), 3, "{:?}", bad.problems);
        let missing_stamp = evaluate(Ok(1), Ok(1.0), Some(Err("backup: can't read".into())), &c);
        assert!(!missing_stamp.ok);
        // No backup stamp configured: the backup isn't checked.
        assert!(evaluate(Ok(1), Ok(1.0), None, &c).ok);
    }

    #[tokio::test]
    async fn real_disk_and_stamp() {
        let mut c = config().await;
        let used = disk_used_percent(&c.data_dir).unwrap();
        assert!((0.0..=100.0).contains(&used));
        let stamp = c.data_dir.join("last-backup");
        std::fs::write(&stamp, format!("{}\n", chrono::Utc::now().to_rfc3339())).unwrap();
        c.backup_stamp = Some(stamp);
        let consolidated = c.data_dir.join("last-consolidation");
        let two_days_ago = chrono::Utc::now() - chrono::Duration::hours(48);
        std::fs::write(&consolidated, two_days_ago.to_rfc3339()).unwrap();
        c.consolidation_stamp = Some(consolidated.clone());
        let health = Health::new(c);
        let (status, Json(report)) = handler(State(health.clone())).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{report:?}");
        assert!(report.backup_age_hours.unwrap() < 0.1);
        assert_eq!(report.consolidation_age_hours, Some(48.0));
        assert!(report.problems[0].starts_with("last consolidation was 48.0 hours ago"));
        std::fs::write(&consolidated, chrono::Utc::now().to_rfc3339()).unwrap();
        let (status, Json(report)) = handler(State(health)).await;
        assert_eq!(status, StatusCode::OK, "{report:?}");
    }
}
