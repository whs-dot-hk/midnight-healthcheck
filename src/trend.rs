//! Is it *moving*?
//!
//! A point-in-time reading cannot tell a node that is healthily catching up from
//! one that is permanently wedged: both report a number below the tip. On preprod
//! a relay pinned to a `cardano-node` too old for the chain's protocol version sat
//! at `syncProgress 99.96` indefinitely while `query tip` kept returning
//! well-formed, healthy-looking JSON. The only thing that separated the two cases
//! was whether the block number changed between two observations.
//!
//! So this check keeps the previous observation in a state file and compares. A
//! component that is behind and advancing is `ok` (with a rate and an ETA); one
//! that is behind and *not* advancing is `fail`. That distinction is the whole
//! point of the check: everything else here exists to serve it.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::nodes;

/// Below this the delta is dominated by jitter rather than progress, so no verdict
/// is reached. A timer firing every 5 min comfortably clears it; two runs typed
/// back to back do not, and are reported as such instead of guessed at.
const MIN_INTERVAL_SECS: f64 = 45.0;

/// `cardano-cli` reports a percentage; the installer treats this as "at the tip"
const CARDANO_SYNC_OK: f64 = 99.99;
/// The setup script's own gate for starting the validator
const DBSYNC_LAG_OK: i64 = 20;

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("HEALTHCHECK_STATE") {
        return PathBuf::from(p);
    }
    let preferred = Path::new("/var/lib/midnight-healthcheck");
    if fs::create_dir_all(preferred).is_ok() {
        let candidate = preferred.join("state.json");
        // Writability is not implied by the directory existing: a non-root run of
        // the same check must fall back rather than fail
        if fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&candidate)
            .is_ok()
        {
            return candidate;
        }
    }
    PathBuf::from("/tmp/midnight-healthcheck-state.json")
}

fn load_previous(path: &Path) -> Option<Value> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_current(path: &Path, snapshot: &Value) {
    if let Ok(text) = serde_json::to_string(snapshot) {
        let _ = fs::write(path, text);
    }
}

fn as_i64(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| x.as_i64())
}

fn as_f64(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(|x| x.as_f64())
}

/// `1 h 04 m`, `2 d 07 h` — deliberately coarse, because an ETA derived from a
/// single interval is an order of magnitude, not a promise
fn human_eta(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "unknown".into();
    }
    let total = seconds as u64;
    let d = total / 86_400;
    let h = (total % 86_400) / 3600;
    let m = (total % 3600) / 60;
    if d > 0 {
        format!("{d} d {h:02} h")
    } else if h > 0 {
        format!("{h} h {m:02} m")
    } else {
        format!("{m} m")
    }
}

/// One component's verdict, plus the numbers behind it
struct Verdict {
    status: &'static str,
    detail: String,
    extra: Value,
}

/// The core rule. `behind` says the component has further to go; `remaining` is how
/// much, when it is known.
fn judge(
    label: &str,
    prev: Option<i64>,
    now: Option<i64>,
    dt: f64,
    behind: bool,
    remaining: Option<i64>,
) -> Verdict {
    let now_v = match now {
        Some(v) => v,
        None => {
            return Verdict {
                status: "warn",
                detail: format!("{label}: could not be read"),
                extra: json!({ "block": Value::Null }),
            }
        }
    };

    if !behind {
        return Verdict {
            status: "ok",
            detail: format!("{label}: at the tip (block {now_v})"),
            extra: json!({ "block": now_v, "advancing": true, "at_tip": true }),
        };
    }

    let (prev_v, dt) = match (prev, dt) {
        (Some(p), d) if d >= MIN_INTERVAL_SECS => (p, d),
        _ => {
            return Verdict {
                status: "ok",
                detail: format!(
                    "{label}: behind at block {now_v}; no verdict yet (need {MIN_INTERVAL_SECS:.0}s between runs)"
                ),
                extra: json!({ "block": now_v, "advancing": Value::Null, "at_tip": false }),
            }
        }
    };

    let delta = now_v - prev_v;
    if delta <= 0 {
        // The case this whole module exists for
        return Verdict {
            status: "fail",
            detail: format!(
                "{label}: STALLED at block {now_v} — no progress in {dt:.0}s while still behind the tip"
            ),
            extra: json!({
                "block": now_v,
                "previous_block": prev_v,
                "advancing": false,
                "at_tip": false,
                "interval_secs": dt,
            }),
        };
    }

    let rate = delta as f64 / dt;
    let eta = remaining.map(|r| human_eta(r as f64 / rate.max(f64::MIN_POSITIVE)));
    Verdict {
        status: "ok",
        detail: match (&eta, remaining) {
            (Some(e), Some(r)) => {
                format!("{label}: catching up, {rate:.1} blocks/s, {r} to go, ~{e}")
            }
            _ => format!("{label}: catching up, {rate:.1} blocks/s"),
        },
        extra: json!({
            "block": now_v,
            "previous_block": prev_v,
            "advancing": true,
            "at_tip": false,
            "blocks_per_second": (rate * 10.0).round() / 10.0,
            "remaining_blocks": remaining,
            "eta": eta,
            "interval_secs": dt,
        }),
    }
}

/// `sudo` resets PATH to its `secure_path`, which does not include the service
/// user's `~/.local/bin` where the installer puts these binaries — so the binary
/// has to be named by absolute path, not looked up.
pub(crate) fn resolve_binary(params: &Value, key: &str, env_key: &str, candidates: &[&str]) -> String {
    let configured = nodes::param_str(params, key, env_key, "");
    if !configured.is_empty() {
        return configured;
    }
    for c in candidates {
        if Path::new(c).exists() {
            return (*c).to_string();
        }
    }
    // Fall back to a bare name so the failure names the binary rather than a path
    candidates
        .last()
        .map(|c| {
            Path::new(c)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
        })
        .unwrap_or(None)
        .unwrap_or_else(|| "cardano-cli".into())
}

fn cardano_now(params: &Value) -> (Option<i64>, Option<f64>) {
    let socket = nodes::param_str(
        params,
        "socket",
        "CARDANO_NODE_SOCKET_PATH",
        "/data/cardano/db/node.socket",
    );
    let user = nodes::param_str(params, "cardano_user", "CARDANO_USER", "midnight");
    let magic = nodes::param_str(params, "testnet_magic", "CARDANO_TESTNET_MAGIC", "1");
    let cli = resolve_binary(
        params,
        "cardano_cli",
        "CARDANO_CLI",
        &[
            "/home/midnight/.local/bin/cardano-cli",
            "/usr/local/bin/cardano-cli",
            "/usr/bin/cardano-cli",
        ],
    );
    // Run as the service user: the socket is not world-accessible
    let out = nodes::run_cmd(
        "sudo",
        &[
            "-n",
            "-u",
            &user,
            "env",
            &format!("CARDANO_NODE_SOCKET_PATH={socket}"),
            &cli,
            "query",
            "tip",
            "--testnet-magic",
            &magic,
        ],
        &[],
    );
    let Ok(text) = out else { return (None, None) };
    let Ok(tip): Result<Value, _> = serde_json::from_str(text.trim()) else {
        return (None, None);
    };
    let block = tip.get("block").and_then(|v| v.as_i64());
    let progress = tip.get("syncProgress").and_then(|v| match v {
        Value::String(s) => s.parse::<f64>().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    });
    (block, progress)
}

fn dbsync_now(params: &Value) -> Option<i64> {
    let db = nodes::param_str(params, "db_name", "PGDATABASE", "cexplorer");
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
            "SELECT COALESCE(MAX(block_no),0) FROM block;",
        ],
        &[],
    )
    .ok()?;
    out.trim().parse::<i64>().ok()
}

fn midnight_now(params: &Value) -> (Option<i64>, Option<i64>, Option<bool>) {
    let url = crate::verify::midnight_rpc_url(params);
    let sync = nodes::rpc_call(&url, "system_syncState", json!([])).ok();
    let health = nodes::rpc_call(&url, "system_health", json!([])).ok();
    let current = sync
        .as_ref()
        .and_then(|s| nodes::hex_u64(s.get("currentBlock").unwrap_or(&Value::Null)))
        .map(|v| v as i64);
    let highest = sync
        .as_ref()
        .and_then(|s| nodes::hex_u64(s.get("highestBlock").unwrap_or(&Value::Null)))
        .map(|v| v as i64);
    let syncing = health
        .as_ref()
        .and_then(|h| h.get("isSyncing").and_then(|v| v.as_bool()));
    (current, highest, syncing)
}

pub fn check(params: &Value) -> Value {
    let path = state_path();
    let previous = load_previous(&path);
    let now = now_unix();
    let dt = previous
        .as_ref()
        .and_then(|p| as_f64(p, "unix"))
        .map(|t| now - t)
        .unwrap_or(0.0);

    let (cardano_block, cardano_progress) = cardano_now(params);
    let dbsync_block = dbsync_now(params);
    let (mid_best, mid_target, mid_syncing) = midnight_now(params);

    let snapshot = json!({
        "unix": now,
        "cardano_block": cardano_block,
        "dbsync_block": dbsync_block,
        "midnight_best": mid_best,
    });

    let mut verdicts = Vec::new();

    verdicts.push(judge(
        "cardano-node",
        previous.as_ref().and_then(|p| as_i64(p, "cardano_block")),
        cardano_block,
        dt,
        cardano_progress
            .map(|p| p < CARDANO_SYNC_OK)
            .unwrap_or(false),
        None,
    ));

    // db-sync's target is the relay's own tip, so it is only meaningful once the
    // relay has one
    let dbsync_remaining = match (cardano_block, dbsync_block) {
        (Some(c), Some(d)) if c > d => Some(c - d),
        _ => None,
    };
    verdicts.push(judge(
        "cardano-db-sync",
        previous.as_ref().and_then(|p| as_i64(p, "dbsync_block")),
        dbsync_block,
        dt,
        dbsync_remaining.map(|r| r > DBSYNC_LAG_OK).unwrap_or(false),
        dbsync_remaining,
    ));

    let mid_remaining = match (mid_target, mid_best) {
        (Some(t), Some(b)) if t > b => Some(t - b),
        _ => None,
    };
    verdicts.push(judge(
        "midnight-node",
        previous.as_ref().and_then(|p| as_i64(p, "midnight_best")),
        mid_best,
        dt,
        mid_syncing.unwrap_or(false),
        mid_remaining,
    ));

    save_current(&path, &snapshot);

    let status = if verdicts.iter().any(|v| v.status == "fail") {
        "fail"
    } else if verdicts.iter().any(|v| v.status == "warn") {
        "warn"
    } else {
        "ok"
    };

    // Only the components that have something to say reach the summary line
    let reason = if status == "ok" {
        verdicts
            .iter()
            .map(|v| v.detail.clone())
            .collect::<Vec<_>>()
            .join("; ")
    } else {
        verdicts
            .iter()
            .filter(|v| v.status != "ok")
            .map(|v| v.detail.clone())
            .collect::<Vec<_>>()
            .join("; ")
    };

    let mut components = serde_json::Map::new();
    for (name, v) in ["cardano_node", "cardano_db_sync", "midnight_node"]
        .iter()
        .zip(verdicts.iter())
    {
        let mut entry = v.extra.clone();
        entry["status"] = json!(v.status);
        entry["detail"] = json!(v.detail);
        components.insert((*name).to_string(), entry);
    }

    crate::wrap(
        "progress",
        status,
        &reason,
        json!({
            "state_file": path.display().to_string(),
            "interval_secs": if dt > 0.0 { json!((dt * 10.0).round() / 10.0) } else { Value::Null },
            "first_run": previous.is_none(),
            "cardano_sync_progress": cardano_progress,
            "components": Value::Object(components),
        }),
    )
}
