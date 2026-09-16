//! Is it *moving*?
//!
//! A point-in-time reading cannot tell a node that is healthily catching up from
//! one that is permanently wedged: both report a number below the tip. On preprod
//! a relay pinned to a `cardano-node` too old for the chain's protocol version sat
//! at `syncProgress 99.96` indefinitely while `query tip` kept returning
//! well-formed, healthy-looking JSON. The only thing that separated the two cases
//! was whether the block number changed between two observations.
//!
//! So this module keeps the previous observation of each component in a state
//! file and compares. Behind and advancing is `ok`, with a rate and an ETA; behind
//! and *not* advancing is `fail`. Everything else here exists to make that verdict
//! trustworthy: a reading it could not take is reported as exactly that, never as
//! "at the tip"; a baseline is only replaced once it has served a verdict, so
//! polling faster than the interval cannot starve detection; and a block number
//! that went *backwards* is a restart or rollback, not a stall.
//!
//! Each component has its own slot with its own timestamp, so checks that share
//! the file cannot trip over each other's baselines.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::nodes;

/// Below this the delta is dominated by jitter rather than progress, so no verdict
/// is reached — and, crucially, the existing baseline is kept so that the next run
/// still has something old enough to compare against.
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

/// A predictable path under a world-writable `/tmp` is a symlink target waiting to
/// happen for a tool that runs as root, so the fallback is the invoking user's own
/// state directory instead.
fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("HEALTHCHECK_STATE") {
        return PathBuf::from(p);
    }
    let preferred = Path::new("/var/lib/midnight-healthcheck");
    if fs::create_dir_all(preferred).is_ok() {
        let candidate = preferred.join("state.json");
        if fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&candidate)
            .is_ok()
        {
            return candidate;
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let dir = Path::new(&home).join(".local/state/midnight-healthcheck");
    let _ = fs::create_dir_all(&dir);
    dir.join("state.json")
}

fn load_state(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .filter(|v: &Value| v.is_object())
        .unwrap_or_else(|| json!({}))
}

fn save_state(path: &Path, state: &Value) {
    if let Ok(text) = serde_json::to_string(state) {
        let _ = fs::write(path, text);
    }
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

pub(crate) struct Verdict {
    pub(crate) status: &'static str,
    pub(crate) detail: String,
    pub(crate) extra: Value,
}

/// What one component looks like right now
pub(crate) struct Reading {
    pub(crate) block: Option<i64>,
    /// `None` means the sync state could not be determined — which is reported as
    /// such, never rounded to "at the tip"
    pub(crate) behind: Option<bool>,
    pub(crate) remaining: Option<i64>,
}

/// The core rule. Returns the verdict and, when the baseline should move, the new
/// baseline block; `None` keeps whatever was stored.
fn judge(
    label: &str,
    prev: Option<(f64, i64)>,
    now_unix: f64,
    reading: &Reading,
) -> (Verdict, Option<i64>) {
    let Some(now_v) = reading.block else {
        return (
            Verdict {
                status: "warn",
                detail: format!("{label}: could not be read"),
                extra: json!({ "block": Value::Null }),
            },
            None,
        );
    };

    let Some(behind) = reading.behind else {
        // A block number without knowing whether it is the tip is still worth
        // keeping as a baseline, but is not a verdict
        return (
            Verdict {
                status: "warn",
                detail: format!("{label}: at block {now_v}, but whether it is behind the tip could not be determined"),
                extra: json!({ "block": now_v, "advancing": Value::Null, "at_tip": Value::Null }),
            },
            Some(now_v),
        );
    };

    if !behind {
        return (
            Verdict {
                status: "ok",
                detail: format!("{label}: at the tip (block {now_v})"),
                extra: json!({ "block": now_v, "advancing": true, "at_tip": true }),
            },
            Some(now_v),
        );
    }

    let Some((prev_unix, prev_v)) = prev else {
        return (
            Verdict {
                status: "ok",
                detail: format!(
                    "{label}: behind at block {now_v}; baseline recorded, verdict on the next run"
                ),
                extra: json!({ "block": now_v, "advancing": Value::Null, "at_tip": false }),
            },
            Some(now_v),
        );
    };

    let dt = now_unix - prev_unix;
    if dt < MIN_INTERVAL_SECS {
        // Too soon to judge — and the old baseline is deliberately kept
        return (
            Verdict {
                status: "ok",
                detail: format!(
                    "{label}: behind at block {now_v}; no verdict yet ({dt:.0}s since baseline, need {MIN_INTERVAL_SECS:.0}s)"
                ),
                extra: json!({ "block": now_v, "advancing": Value::Null, "at_tip": false, "interval_secs": dt }),
            },
            None,
        );
    }

    let delta = now_v - prev_v;
    if delta < 0 {
        // Going backwards is a restart from an earlier point or a rollback to a
        // stable block: the very thing a genesis fix prescribes, and routine for
        // db-sync on restart. It is not a stall, and the baseline must reset.
        return (
            Verdict {
                status: "warn",
                detail: format!(
                    "{label}: block went backwards ({prev_v} → {now_v}) — restarted or rolled back; baseline reset"
                ),
                extra: json!({
                    "block": now_v, "previous_block": prev_v, "advancing": Value::Null,
                    "at_tip": false, "rolled_back": true, "interval_secs": dt,
                }),
            },
            Some(now_v),
        );
    }

    if delta == 0 {
        // The case this whole module exists for
        return (
            Verdict {
                status: "fail",
                detail: format!(
                    "{label}: STALLED at block {now_v} — no progress in {dt:.0}s while still behind the tip"
                ),
                extra: json!({
                    "block": now_v, "previous_block": prev_v, "advancing": false,
                    "at_tip": false, "interval_secs": dt,
                }),
            },
            Some(now_v),
        );
    }

    let rate = delta as f64 / dt;
    let eta = reading.remaining.map(|r| human_eta(r as f64 / rate));
    (
        Verdict {
            status: "ok",
            detail: match (&eta, reading.remaining) {
                (Some(e), Some(r)) => {
                    format!("{label}: catching up, {rate:.1} blocks/s, {r} to go, ~{e}")
                }
                _ => format!("{label}: catching up, {rate:.1} blocks/s"),
            },
            extra: json!({
                "block": now_v, "previous_block": prev_v, "advancing": true, "at_tip": false,
                "blocks_per_second": (rate * 10.0).round() / 10.0,
                "remaining_blocks": reading.remaining, "eta": eta, "interval_secs": dt,
            }),
        },
        Some(now_v),
    )
}

/// Judge one component against its own slot in the state file, and persist the
/// slot if the verdict moved the baseline. Self-contained, so any check may call
/// it for any slot without disturbing another's.
pub(crate) fn component(slot: &str, label: &str, reading: &Reading) -> Verdict {
    let path = state_path();
    let mut state = load_state(&path);
    let now = now_unix();
    let prev = state
        .get(slot)
        .and_then(|s| Some((s.get("unix")?.as_f64()?, s.get("block")?.as_i64()?)));
    let (verdict, new_baseline) = judge(label, prev, now, reading);
    if let Some(block) = new_baseline {
        state[slot] = json!({ "unix": now, "block": block });
        save_state(&path, &state);
    }
    verdict
}

fn user_home(user: &str) -> Option<String> {
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.len() >= 6 && f[0] == user).then(|| f[5].to_string())
        })
        .next()
}

/// The user a unit runs as, which is where the installer puts its binaries. The
/// setup script defaults this to whoever ran it, so it is read rather than assumed.
fn unit_user(unit: &str) -> Option<String> {
    nodes::run_cmd("systemctl", &["show", unit, "-p", "User", "--value"], &[])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `sudo` resets PATH to its `secure_path`, which does not include the service
/// user's `~/.local/bin` — so the binary is named by absolute path, not looked up.
pub(crate) fn resolve_binary(
    params: &Value,
    key: &str,
    env_key: &str,
    candidates: &[String],
) -> String {
    let configured = nodes::param_str(params, key, env_key, "");
    if !configured.is_empty() {
        return configured;
    }
    for c in candidates {
        if Path::new(c).exists() {
            return c.clone();
        }
    }
    candidates
        .last()
        .and_then(|c| {
            Path::new(c)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "cardano-cli".into())
}

/// Candidate locations for `cardano-cli`, most specific first: the service user's
/// `~/.local/bin` (where the installer puts it), then the system directories
pub(crate) fn cardano_cli_candidates(user: &str) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(home) = user_home(user) {
        v.push(format!("{home}/.local/bin/cardano-cli"));
    }
    v.push("/usr/local/bin/cardano-cli".into());
    v.push("/usr/bin/cardano-cli".into());
    v
}

pub(crate) fn cardano_user(params: &Value) -> String {
    let configured = nodes::param_str(params, "cardano_user", "CARDANO_USER", "");
    if !configured.is_empty() {
        return configured;
    }
    unit_user("cardano-node.service").unwrap_or_else(|| "root".into())
}

fn cardano_reading(params: &Value) -> Reading {
    let socket = nodes::param_str(
        params,
        "socket",
        "CARDANO_NODE_SOCKET_PATH",
        "/data/cardano/db/node.socket",
    );
    let user = cardano_user(params);
    let cli = resolve_binary(
        params,
        "cardano_cli",
        "CARDANO_CLI",
        &cardano_cli_candidates(&user),
    );
    let socket_env = format!("CARDANO_NODE_SOCKET_PATH={socket}");
    let mut args: Vec<String> = vec![
        "-n".into(),
        "-u".into(),
        user,
        "env".into(),
        socket_env,
        cli,
        "query".into(),
        "tip".into(),
    ];
    args.extend(nodes::cardano_network_args(params));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let Ok(text) = nodes::run_cmd("sudo", &refs, &[]) else {
        return Reading {
            block: None,
            behind: None,
            remaining: None,
        };
    };
    let Ok(tip): Result<Value, _> = serde_json::from_str(text.trim()) else {
        return Reading {
            block: None,
            behind: None,
            remaining: None,
        };
    };
    let block = tip.get("block").and_then(|v| v.as_i64());
    let progress = tip.get("syncProgress").and_then(|v| match v {
        Value::String(s) => s.parse::<f64>().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    });
    Reading {
        block,
        behind: progress.map(|p| p < CARDANO_SYNC_OK),
        remaining: None,
    }
}

fn dbsync_reading(params: &Value, cardano_block: Option<i64>) -> Reading {
    let db = nodes::db_name(params);
    let block = nodes::run_cmd(
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
    .ok()
    .and_then(|out| out.trim().parse::<i64>().ok());
    // db-sync's target is the relay's tip: without one, its distance is unknown
    let remaining = match (cardano_block, block) {
        (Some(c), Some(d)) => Some((c - d).max(0)),
        _ => None,
    };
    Reading {
        block,
        behind: remaining.map(|r| r > DBSYNC_LAG_OK),
        remaining,
    }
}

pub(crate) fn midnight_reading(params: &Value) -> Reading {
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
    let remaining = match (highest, current) {
        (Some(t), Some(b)) => Some((t - b).max(0)),
        _ => None,
    };
    Reading {
        block: current,
        behind: syncing,
        remaining,
    }
}

pub fn check(params: &Value) -> Value {
    let cardano = cardano_reading(params);
    let dbsync = dbsync_reading(params, cardano.block);
    let midnight = midnight_reading(params);

    let verdicts = [
        (
            "cardano_node",
            component("cardano_node", "cardano-node", &cardano),
        ),
        (
            "cardano_db_sync",
            component("cardano_db_sync", "cardano-db-sync", &dbsync),
        ),
        (
            "midnight_node",
            component("midnight_node", "midnight-node", &midnight),
        ),
    ];

    let status = if verdicts.iter().any(|(_, v)| v.status == "fail") {
        "fail"
    } else if verdicts.iter().any(|(_, v)| v.status == "warn") {
        "warn"
    } else {
        "ok"
    };

    // Only the components with something to say reach the summary line
    let reason = verdicts
        .iter()
        .filter(|(_, v)| status == "ok" || v.status != "ok")
        .map(|(_, v)| v.detail.clone())
        .collect::<Vec<_>>()
        .join("; ");

    let mut components = serde_json::Map::new();
    for (name, v) in &verdicts {
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
            "state_file": state_path().display().to_string(),
            "components": Value::Object(components),
        }),
    )
}
