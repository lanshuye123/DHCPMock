//! Lease-table JSON export (`--lease-file`, SIGUSR1) and counters.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::dhcp::packet::MacAddr;
use crate::error::{AppError, Result};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct Counters {
    pub received: u64,
    pub discover: u64,
    pub offer: u64,
    pub request: u64,
    pub ack: u64,
    pub nak: u64,
    pub release: u64,
    pub decline: u64,
    pub inform: u64,
    pub dropped: u64,
    pub parse_errors: u64,
    pub send_errors: u64,
    pub sent: u64,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub iface: String,
    pub ifindex: u32,
    pub server_ip: String,
    pub prefix: u8,
    pub network: String,
    pub broadcast: String,
    pub pool_start: String,
    pub pool_end: String,
    pub lease_seconds: u32,
    pub uptime_seconds: u64,
    pub generated_at_unix: u64,
    pub leases: Vec<LeaseRow>,
    pub declined: Vec<DeclinedRow>,
    pub counters: Counters,
}

#[derive(Debug, Serialize)]
pub struct LeaseRow {
    pub mac: MacAddr,
    pub ip: String,
    pub state: String,
    pub expires_in_secs: u64,
}

#[derive(Debug, Serialize)]
pub struct DeclinedRow {
    pub ip: String,
    pub cooldown_secs: u64,
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Write the snapshot atomically (temp file + rename in the same directory).
pub fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(snapshot)
        .map_err(|e| AppError::Runtime(format!("serialize lease snapshot: {e}")))?;
    data.push(b'\n');

    let file_name = path.file_name().ok_or_else(|| {
        AppError::Config(format!("invalid --lease-file path: {}", path.display()))
    })?;
    let tmp = path.with_file_name(format!(
        ".{}.tmp.{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));

    std::fs::write(&tmp, &data)
        .map_err(|e| AppError::Runtime(format!("write {}: {e}", tmp.display())))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AppError::Runtime(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> Snapshot {
        Snapshot {
            schema_version: SCHEMA_VERSION,
            iface: "eth0".into(),
            ifindex: 2,
            server_ip: "10.0.0.1".into(),
            prefix: 24,
            network: "10.0.0.0".into(),
            broadcast: "10.0.0.255".into(),
            pool_start: "10.0.0.2".into(),
            pool_end: "10.0.0.254".into(),
            lease_seconds: 3_600,
            uptime_seconds: 1,
            generated_at_unix: unix_now(),
            leases: vec![LeaseRow {
                mac: "aa:bb:cc:dd:ee:ff".parse().unwrap(),
                ip: "10.0.0.2".into(),
                state: "bound".into(),
                expires_in_secs: 3_500,
            }],
            declined: vec![DeclinedRow {
                ip: "10.0.0.3".into(),
                cooldown_secs: 120,
            }],
            counters: Counters {
                received: 2,
                discover: 1,
                ..Counters::default()
            },
        }
    }

    fn documented_schema() -> serde_json::Value {
        serde_json::from_str(include_str!("../../docs/lease-snapshot.schema.json"))
            .expect("documented JSON schema must be valid JSON")
    }

    #[test]
    fn writes_valid_json_atomically() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("dhcp-mock-report-test-{}.json", std::process::id()));
        let snapshot = sample_snapshot();
        write_snapshot(&path, &snapshot).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["iface"], "eth0");
        assert_eq!(value["leases"][0]["mac"], "aa:bb:cc:dd:ee:ff");
        assert_eq!(value["declined"][0]["ip"], "10.0.0.3");
        assert_eq!(value["counters"]["discover"], 1);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn documented_example_matches_schema() {
        let example: serde_json::Value =
            serde_json::from_str(include_str!("../../docs/lease-snapshot.example.json"))
                .expect("documented example must be valid JSON");
        let schema = documented_schema();

        for key in schema["required"].as_array().unwrap() {
            let key = key.as_str().unwrap();
            assert!(
                example.get(key).is_some(),
                "documented example is missing top-level key `{key}`"
            );
        }
        for key in schema["$defs"]["lease"]["required"].as_array().unwrap() {
            let key = key.as_str().unwrap();
            assert!(
                example["leases"][0].get(key).is_some(),
                "documented example is missing lease field `{key}`"
            );
        }
        for key in schema["$defs"]["counters"]["required"].as_array().unwrap() {
            let key = key.as_str().unwrap();
            assert!(
                example["counters"].get(key).is_some(),
                "documented example is missing counter `{key}`"
            );
        }
    }

    /// Keep the published schema and the actual serialized snapshot in sync.
    #[test]
    fn schema_matches_serialized_snapshot() {
        let schema = documented_schema();
        let value = serde_json::to_value(sample_snapshot()).unwrap();

        for key in schema["required"].as_array().unwrap() {
            let key = key.as_str().unwrap();
            assert!(
                value.get(key).is_some(),
                "schema requires `{key}` but Snapshot does not emit it"
            );
        }
        for key in value.as_object().unwrap().keys() {
            assert!(
                schema["properties"].get(key).is_some(),
                "Snapshot emits undocumented key `{key}`"
            );
        }

        let counters = &schema["$defs"]["counters"];
        for key in counters["required"].as_array().unwrap() {
            let key = key.as_str().unwrap();
            assert!(
                value["counters"].get(key).is_some(),
                "schema requires counter `{key}` but Counters does not emit it"
            );
        }
        for key in value["counters"].as_object().unwrap().keys() {
            assert!(
                counters["properties"].get(key).is_some(),
                "Counters emits undocumented key `{key}`"
            );
        }
    }
}
