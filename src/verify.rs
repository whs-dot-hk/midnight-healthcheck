//! Checks that compare this host against something outside it.
//!
//! A node can be `active`, peered, and confidently wrong. The three failures here
//! are all ones that a local-only check reports as healthy:
//!
//! * **Genesis drift** — a node booted from a chain spec that is not the live
//!   chain syncs happily on a chain of its own, with `Role: AUTHORITY` and peers.
//!   Only block 0, compared against the network, gives it away.
//! * **A stale running binary** — `systemctl enable --now` does not restart a unit
//!   that is already active, so a version bump leaves the new binary on disk and
//!   the old one in memory. `/proc/<pid>/exe` still points at the replaced inode.
//! * **Secrets that drifted open** — key material is only as good as its mode.

use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::nodes;
use crate::wrap;

const DEFAULT_NETWORK_RPC: &str = "https://rpc.preprod.midnight.network";

/// The built-in client speaks plain HTTP only, which is fine for a loopback RPC but
/// not for the public one. Rather than pull a TLS stack in for a single call, defer
/// to `curl` — the same way this crate already defers to `cardano-cli` and `psql`,
/// and a tool the setup script installs anyway.
fn rpc_via_curl(url: &str, method: &str, params: Value) -> Result<Value, String> {
    let payload =
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
    let out = nodes::run_cmd(
        "curl",
        &[
            "-sS",
            "--max-time",
            "10",
            "-H",
            "Content-Type: application/json",
            "-d",
            &payload,
            url,
        ],
        &[],
    )?;
    let parsed: Value =
        serde_json::from_str(out.trim()).map_err(|e| format!("bad JSON from {url}: {e}"))?;
    if let Some(err) = parsed.get("error") {
        return Err(err.to_string());
    }
    parsed
        .get("result")
        .cloned()
        .ok_or_else(|| format!("no result field from {url}"))
}

/// Same call either way; the scheme decides the transport
fn rpc_any(url: &str, method: &str, params: Value) -> Result<Value, String> {
    if url.starts_with("https://") {
        rpc_via_curl(url, method, params)
    } else {
        nodes::rpc_call(url, method, params)
    }
}

/// Where the local node's RPC is.
///
/// An explicit setting is always honoured. Absent one, probe: the installer's unit
/// uses 9944 while the interactive setup script passes `--rpc-port 9933`, and a
/// healthcheck pointed at the wrong port reports a dead node that is running
/// perfectly well — the most expensive kind of noise there is.
pub fn midnight_rpc_url(params: &Value) -> String {
    if let Some(u) = params.get("url").and_then(|v| v.as_str()) {
        if !u.is_empty() {
            return u.to_string();
        }
    }
    if let Ok(u) = std::env::var("MIDNIGHT_RPC_URL") {
        if !u.is_empty() {
            return u;
        }
    }
    // Several checks need this; probing once per process rather than once per
    // caller keeps a hung RPC from costing a timeout at every call site
    static PROBED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PROBED
        .get_or_init(|| {
            for port in [9944u16, 9933] {
                let candidate = format!("http://127.0.0.1:{port}");
                if nodes::rpc_call(&candidate, "system_health", json!([])).is_ok() {
                    return candidate;
                }
            }
            "http://127.0.0.1:9944".into()
        })
        .clone()
}

/// On its first start the node builds its own indexes on the shared `cexplorer`
/// and opens no ports until they are done. An unreachable RPC then means "starting",
/// not "broken", and saying so is the difference between a useful alert and a
/// false alarm every time a node is rebuilt.
pub fn index_build_progress(params: &Value) -> Option<String> {
    // A build only explains an unreachable RPC if the node is actually running —
    // a crashed unit is down, whatever db-sync happens to be doing at the time
    if nodes::unit_active("midnight-node.service") != Some(true) {
        return None;
    }
    let db = nodes::db_name(params);
    // Only the indexes midnight-node itself creates on first start; db-sync's own
    // index work, which can run for hours, must not be mistaken for it
    let out = nodes::run_cmd(
        "sudo",
        &[
            "-n",
            "-u",
            "postgres",
            "psql",
            "-d",
            &db,
            "-tAc",
            "SELECT p.phase, p.blocks_done, p.blocks_total FROM pg_stat_progress_create_index p \
             JOIN pg_class c ON c.oid = p.index_relid \
             WHERE p.datid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
               AND c.relname IN ('idx_tx_out_address','idx_ma_tx_out_ident','idx_multi_asset_policy_name_hex') \
             LIMIT 1;",
        ],
        &[],
    )
    .ok()?;
    let line = out.trim();
    if line.is_empty() {
        return None;
    }
    let cols: Vec<&str> = line.split('|').collect();
    match cols.as_slice() {
        [phase, done, total] => {
            let pct = match (done.parse::<f64>(), total.parse::<f64>()) {
                (Ok(d), Ok(t)) if t > 0.0 => format!(" ({:.0}%)", d / t * 100.0),
                _ => String::new(),
            };
            Some(format!("{phase}{pct}"))
        }
        _ => Some(line.to_string()),
    }
}

/// `1.0.2-eb71e64e` and `1.0.2` are the same release; the suffix is the build commit
fn release_of(version: &str) -> &str {
    version.split('-').next().unwrap_or(version)
}

pub fn check_chain_identity(params: &Value) -> Value {
    let local_url = midnight_rpc_url(params);
    let network_url = nodes::param_str(
        params,
        "network_url",
        "MIDNIGHT_NETWORK_RPC_URL",
        DEFAULT_NETWORK_RPC,
    );

    let local_genesis = nodes::rpc_call(&local_url, "chain_getBlockHash", json!([0]))
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()));
    let local_version = nodes::rpc_call(&local_url, "system_version", json!([]))
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()));

    if local_genesis.is_none() {
        // Distinguish "still starting" from "down" before raising anything
        let (status, reason) = match index_build_progress(params) {
            Some(p) => (
                "warn",
                format!("local RPC not up yet: first-start index build in progress — {p}"),
            ),
            None => (
                "fail",
                format!("local RPC unreachable at {local_url}; cannot verify which chain this node is on"),
            ),
        };
        return wrap(
            "chain_identity",
            status,
            &reason,
            json!({ "local_url": local_url, "network_url": network_url }),
        );
    }

    let network_genesis = rpc_any(&network_url, "chain_getBlockHash", json!([0]))
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()));
    let network_version = rpc_any(&network_url, "system_version", json!([]))
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()));

    let extra = json!({
        "local_url": local_url,
        "network_url": network_url,
        "local_genesis": local_genesis,
        "network_genesis": network_genesis,
        "local_version": local_version,
        "network_version": network_version,
    });

    // A network we cannot reach is a network we cannot compare against. That is a
    // gap in knowledge, not evidence of a fault here, so it must not read as one.
    let Some(net_genesis) = network_genesis.clone() else {
        return wrap(
            "chain_identity",
            "warn",
            &format!(
                "could not reach {network_url} to verify genesis; local genesis {}",
                local_genesis.unwrap_or_default()
            ),
            extra,
        );
    };

    if local_genesis.as_deref() != Some(net_genesis.as_str()) {
        return wrap(
            "chain_identity",
            "fail",
            &format!(
                "GENESIS MISMATCH — this node is on a different chain from {network_url}. local {} vs network {net_genesis}. Fix the chain spec, then delete only paritydb (network/ and keystore/ must survive) and resync",
                local_genesis.clone().unwrap_or_default()
            ),
            extra,
        );
    }

    match (local_version.as_deref(), network_version.as_deref()) {
        (Some(l), Some(n)) if release_of(l) != release_of(n) => wrap(
            "chain_identity",
            "warn",
            &format!("genesis matches, but this node runs {l} while the network runs {n}"),
            extra,
        ),
        _ => wrap(
            "chain_identity",
            "ok",
            &format!(
                "on the same chain as {network_url} (genesis {}…, {})",
                net_genesis.chars().take(14).collect::<String>(),
                local_version
                    .clone()
                    .unwrap_or_else(|| "version unknown".into())
            ),
            extra,
        ),
    }
}

fn main_pid(unit: &str) -> Option<u32> {
    let out = nodes::run_cmd(
        "systemctl",
        &["show", unit, "-p", "MainPID", "--value"],
        &[],
    )
    .ok()?;
    let pid: u32 = out.trim().parse().ok()?;
    if pid == 0 {
        None
    } else {
        Some(pid)
    }
}

pub fn check_binaries(params: &Value) -> Value {
    let units: Vec<String> = params
        .get("units")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "cardano-node.service".into(),
                "cardano-db-sync.service".into(),
                "midnight-node.service".into(),
            ]
        });

    let mut stale = Vec::new();
    let mut details = serde_json::Map::new();
    for unit in &units {
        let Some(pid) = main_pid(unit) else {
            details.insert(unit.clone(), json!({ "running": false }));
            continue;
        };
        let link = fs::read_link(format!("/proc/{pid}/exe"))
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        // The kernel appends " (deleted)" once the running image is no longer the
        // file at that path: the binary was replaced and the unit never restarted
        let replaced = link.ends_with(" (deleted)");
        if replaced {
            stale.push(unit.clone());
        }
        details.insert(
            unit.clone(),
            json!({ "running": true, "pid": pid, "exe": link, "binary_replaced_on_disk": replaced }),
        );
    }

    let running: Vec<&String> = units
        .iter()
        .filter(|u| {
            details
                .get(*u)
                .and_then(|d| d.get("running"))
                .and_then(|r| r.as_bool())
                == Some(true)
        })
        .collect();
    if stale.is_empty() {
        let reason = if running.is_empty() {
            "no checked unit is running; nothing to compare".to_string()
        } else {
            format!(
                "{} running unit(s) executing the binary on disk: {}",
                running.len(),
                running
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        wrap(
            "binaries",
            if running.is_empty() { "warn" } else { "ok" },
            &reason,
            json!({ "units": Value::Object(details) }),
        )
    } else {
        wrap(
            "binaries",
            "warn",
            &format!(
                "running a binary that has since been replaced on disk: {}. `systemctl enable --now` does not restart an already-active unit — restart it: sudo systemctl restart {}",
                stale.join(", "),
                stale.join(" ")
            ),
            json!({ "units": Value::Object(details) }),
        )
    }
}

/// path, whether it must exist, and the bits that must NOT be set
struct SecretSpec {
    path: String,
    required: bool,
}

pub fn check_secrets(params: &Value) -> Value {
    let base = nodes::param_str(
        params,
        "midnight_data",
        "MIDNIGHT_NODE_DATA",
        "/data/midnight_node",
    );
    let chain = nodes::param_str(params, "chain_id", "MIDNIGHT_CHAIN_ID", "midnight_preprod");
    let network_dir = format!("{base}/data/chains/{chain}/network");
    let keystore = format!("{base}/data/chains/{chain}/keystore");

    let mut specs: Vec<SecretSpec> = Vec::new();
    for p in [
        format!("{base}/keys"),
        format!("{base}/keys/aura.json"),
        format!("{base}/keys/grandpa.json"),
        format!("{base}/keys/cross_chain.json"),
        format!("{network_dir}/secret_ed25519"),
        keystore.clone(),
        "/data/postgresql/fno-db-credentials.env".to_string(),
    ] {
        specs.push(SecretSpec {
            path: p,
            required: true,
        });
    }
    // Written only once the validator stage has run — which is gated on db-sync
    // reaching the tip and can be hours or days after the keys exist
    for p in [
        format!("{base}/.env"),
        format!("{base}/keys/aura.seed"),
        format!("{base}/keys/grandpa.seed"),
        format!("{base}/keys/cross_chain.seed"),
    ] {
        specs.push(SecretSpec {
            path: p,
            required: false,
        });
    }

    let mut exposed = Vec::new();
    let mut missing = Vec::new();
    let mut listing = serde_json::Map::new();
    for spec in &specs {
        let p = Path::new(&spec.path);
        match fs::metadata(p) {
            Ok(md) => {
                let mode = md.mode() & 0o777;
                // Anything readable by group or other is a finding, file or directory
                let open = mode & 0o077 != 0;
                if open {
                    exposed.push(format!("{} is {:04o}", spec.path, mode));
                }
                listing.insert(
                    spec.path.clone(),
                    json!({ "mode": format!("{mode:04o}"), "uid": md.uid(), "gid": md.gid(), "group_or_world_accessible": open }),
                );
            }
            Err(_) => {
                if spec.required {
                    missing.push(spec.path.clone());
                }
                listing.insert(spec.path.clone(), json!({ "present": false }));
            }
        }
    }

    let extra = json!({ "paths": Value::Object(listing) });
    if !exposed.is_empty() {
        wrap(
            "secrets",
            "fail",
            &format!(
                "key material is readable beyond its owner: {}",
                exposed.join(", ")
            ),
            extra,
        )
    } else if !missing.is_empty() {
        wrap(
            "secrets",
            "fail",
            &format!("missing key material: {}", missing.join(", ")),
            extra,
        )
    } else {
        wrap(
            "secrets",
            "ok",
            "key material present and owner-only",
            extra,
        )
    }
}
