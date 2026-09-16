//! Midnight, cardano-node, cardano-db-sync.
//! Paths/env follow https://github.com/whs-dot-hk/midnight-installer

use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(4);
const MIN_PEERS: u64 = 1;
const DBSYNC_LAG_WARN: i64 = 5;
const DBSYNC_LAG_FAIL: i64 = 20; // installer: at most 20 counts as caught up
const CARDANO_SYNC_OK: f64 = 99.99;
const DEFAULT_SOCKET: &str = "/data/cardano/db/node.socket";
const DEFAULT_CREDS: &str = "/data/postgresql/fno-db-credentials.env";
const DEFAULT_PROM: &str = "http://127.0.0.1:12798/metrics";

pub fn param_str(params: &Value, key: &str, env_key: &str, default: &str) -> String {
    if let Some(s) = params.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    env::var(env_key).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

fn param_u64(params: &Value, key: &str, env_key: &str, default: u64) -> u64 {
    params
        .get(key)
        .and_then(|v| v.as_u64())
        .or_else(|| env::var(env_key).ok().and_then(|s| s.parse().ok()))
        .unwrap_or(default)
}

fn wrap(name: &str, status: &str, reason: &str, extra: Value) -> Value {
    let mut o = json!({
        "name": name,
        "status": status,
        "healthy": status != "fail",
        "reason": reason,
    });
    if let (Some(obj), Value::Object(map)) = (o.as_object_mut(), extra) {
        for (k, v) in map {
            obj.insert(k, v);
        }
    }
    o
}

struct Url {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<Url, String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| format!("only http:// URLs supported (got {url})"))?;
    let (hostport, path) = match rest.split_once('/') {
        Some((hp, p)) => (hp, format!("/{p}")),
        None => (rest, "/".into()),
    };
    let (host, port) = if let Some((h, p)) = hostport.rsplit_once(':') {
        (h.to_string(), p.parse().map_err(|_| format!("bad port in {url}"))?)
    } else {
        (hostport.to_string(), 80)
    };
    if host.is_empty() {
        return Err("empty host".into());
    }
    Ok(Url { host, port, path })
}

fn http_exchange(url: &str, method: &str, content_type: &str, body: &[u8]) -> Result<(u16, String), String> {
    let u = parse_http_url(url)?;
    let mut stream = TcpStream::connect((u.host.as_str(), u.port)).map_err(|e| format!("connect {url}: {e}"))?;
    stream
        .set_read_timeout(Some(HTTP_TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(HTTP_TIMEOUT)))
        .map_err(|e| e.to_string())?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: healthcheck\r\nAccept: */*\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\n\r\n",
        path = u.path, host = u.host, port = u.port, len = body.len(),
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    if !body.is_empty() {
        stream.write_all(body).map_err(|e| e.to_string())?;
    }
    let _ = stream.flush();
    let mut buf = Vec::new();
    stream.take(1_048_576).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").or_else(|| text.split_once("\n\n")).ok_or("invalid HTTP response")?;
    let status: u16 = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((status, rest.to_string()))
}

fn http_get(url: &str) -> Result<(u16, String), String> {
    http_exchange(url, "GET", "text/plain", b"")
}

fn http_post_json(url: &str, payload: &Value) -> Result<Value, String> {
    let body = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let (status, body) = http_exchange(url, "POST", "application/json", &body)?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", body.chars().take(200).collect::<String>()));
    }
    serde_json::from_str(&body).map_err(|e| format!("json: {e}"))
}

fn rpc_call(url: &str, method: &str, params: Value) -> Result<Value, String> {
    let resp = http_post_json(url, &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))?;
    if let Some(err) = resp.get("error") {
        return Err(err.to_string());
    }
    resp.get("result").cloned().ok_or_else(|| "missing result".into())
}

fn processes_matching(needles: &[&str]) -> Vec<Value> {
    let mut found = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else { return found };
    for ent in dir.flatten() {
        let pid: u32 = match ent.file_name().to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let comm = fs::read_to_string(ent.path().join("comm")).unwrap_or_default();
        let comm = comm.trim();
        let cmdline = fs::read(ent.path().join("cmdline"))
            .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
            .unwrap_or_default();
        let hay = format!("{comm} {cmdline}").to_lowercase();
        if needles.iter().any(|n| hay.contains(&n.to_lowercase())) {
            found.push(json!({"pid": pid, "comm": comm, "cmdline": cmdline.trim()}));
        }
    }
    found
}

fn unit_active(unit: &str) -> Option<bool> {
    let out = Command::new("systemctl").args(["is-active", unit]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    Some(s.trim() == "active")
}

fn prom_metric(body: &str, name: &str) -> Option<f64> {
    for line in body.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix(name) else { continue };
        if !(rest.starts_with('{') || rest.starts_with(' ') || rest.starts_with('\t')) {
            continue;
        }
        let val = rest.rsplit_once(|c: char| c.is_whitespace()).map(|(_, v)| v.trim())?;
        if let Ok(n) = val.parse::<f64>() {
            return Some(n);
        }
    }
    None
}

fn run_cmd(bin: &str, args: &[&str], extra_env: &[(&str, String)]) -> Result<String, String> {
    let mut c = Command::new(bin);
    c.args(args);
    for (k, v) in extra_env {
        c.env(k, v);
    }
    let out = c.output().map_err(|e| format!("{bin}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{bin} exited {}: {}", out.status.code().unwrap_or(-1), err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn hex_u64(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(inner) = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return inner.replace("'\\''", "'");
    }
    if let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return inner.replace("\\\"", "\"");
    }
    raw.to_string()
}

fn load_env_file(path: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    let Ok(s) = fs::read_to_string(path) else { return m };
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            m.insert(k.trim().to_string(), unquote(v));
        }
    }
    m
}

fn substrate(name: &str, params: &Value, default_url: &str, env_url: &str, needles: &[&str]) -> Value {
    let url = param_str(params, "url", env_url, default_url);
    let min_peers = param_u64(params, "min_peers", "SUBSTRATE_MIN_PEERS", MIN_PEERS);
    let procs = processes_matching(needles);
    let health = rpc_call(&url, "system_health", json!([]));
    let chain = rpc_call(&url, "system_chain", json!([])).ok();
    let sys_name = rpc_call(&url, "system_name", json!([])).ok();
    let version = rpc_call(&url, "system_version", json!([])).ok();
    let sync = rpc_call(&url, "system_syncState", json!([])).ok();
    let header = rpc_call(&url, "chain_getHeader", json!([Value::Null])).ok();

    let current = sync.as_ref().and_then(|s| hex_u64(s.get("currentBlock").unwrap_or(&Value::Null)));
    let highest = sync.as_ref().and_then(|s| hex_u64(s.get("highestBlock").unwrap_or(&Value::Null)));
    let header_num = header.as_ref().and_then(|s| hex_u64(s.get("number").unwrap_or(&Value::Null)));
    let block_height = header_num.or(current);
    let lag = match (current, highest) {
        (Some(c), Some(hi)) if hi >= c => Some(hi - c),
        _ => None,
    };
    let extra = json!({
        "url": url,
        "chain": chain,
        "system_name": sys_name,
        "version": version,
        "block_height": block_height,
        "current_block": current,
        "highest_block": highest,
        "header_number": header_num,
        "lag_blocks": lag,
        "peers": Value::Null,
        "is_syncing": Value::Null,
        "should_have_peers": Value::Null,
        "processes": procs,
    });

    match health {
        Ok(h) => {
            let peers = h.get("peers").and_then(|v| v.as_u64()).unwrap_or(0);
            let is_syncing = h.get("isSyncing").and_then(|v| v.as_bool()).unwrap_or(false);
            let should_have_peers = h.get("shouldHavePeers").and_then(|v| v.as_bool()).unwrap_or(true);
            let mut extra = extra;
            extra["peers"] = json!(peers);
            extra["is_syncing"] = json!(is_syncing);
            extra["should_have_peers"] = json!(should_have_peers);
            let mut status = "ok";
            let mut reason = match block_height {
                Some(h) => format!("block {h}"),
                None => "rpc ok".into(),
            };
            if should_have_peers && peers < min_peers {
                status = "fail";
                reason = format!("peers {peers} < min {min_peers}");
            } else if is_syncing {
                status = if lag.unwrap_or(0) > 50 { "fail" } else { "warn" };
                reason = format!("syncing (block {:?}, lag {:?})", block_height, lag);
            } else if procs.is_empty() {
                status = "warn";
                reason = "rpc ok but process not found in /proc".into();
            }
            wrap(name, status, &reason, extra)
        }
        Err(e) => wrap(name, "fail", &format!("rpc failed: {e}"), extra),
    }
}

pub fn check_midnight(params: &Value) -> Value {
    let mut r = substrate(
        "midnight",
        params,
        "http://127.0.0.1:9944",
        "MIDNIGHT_RPC_URL",
        &["midnight-node", "midnight", "partner-chains-node"],
    );
    if r.get("status").and_then(|s| s.as_str()) == Some("fail") {
        if let Some(false) = unit_active("midnight-node.service") {
            r["reason"] = json!(format!("{}; midnight-node.service not active", r["reason"].as_str().unwrap_or("")));
        }
    }
    r
}

fn cardano_network_args(params: &Value) -> Vec<String> {
    let magic = param_str(params, "testnet_magic", "CARDANO_TESTNET_MAGIC", "");
    if !magic.is_empty() {
        return vec!["--testnet-magic".into(), magic];
    }
    // midnight-installer default is preprod (magic 1)
    let net = param_str(params, "network", "CARDANO_NETWORK", "preprod");
    match net.as_str() {
        "mainnet" => vec!["--mainnet".into()],
        "preprod" => vec!["--testnet-magic".into(), "1".into()],
        "preview" => vec!["--testnet-magic".into(), "2".into()],
        other => vec!["--testnet-magic".into(), other.into()],
    }
}

pub fn check_cardano_node(params: &Value) -> Value {
    let prom_url = param_str(params, "prometheus_url", "CARDANO_PROMETHEUS_URL", DEFAULT_PROM);
    let socket = param_str(params, "socket", "CARDANO_NODE_SOCKET_PATH", DEFAULT_SOCKET);
    let procs = processes_matching(&["cardano-node"]);
    let unit = unit_active("cardano-node.service");
    let socket_ok = {
        let p = Path::new(&socket);
        p.exists() && fs::metadata(p).map(|m| m.file_type().is_socket()).unwrap_or(true)
    };

    let mut block_height: Option<u64> = None;
    let mut slot: Option<u64> = None;
    let mut epoch: Option<u64> = None;
    let mut density: Option<f64> = None;
    let mut peers: Option<f64> = None;
    let mut sync_progress: Option<f64> = None;
    let mut prom_ok = false;
    let mut reasons: Vec<String> = Vec::new();
    match http_get(&prom_url) {
        Ok((200, body)) => {
            prom_ok = true;
            block_height = prom_metric(&body, "cardano_node_metrics_blockNum_int").map(|n| n as u64);
            slot = prom_metric(&body, "cardano_node_metrics_slotNum_int").map(|n| n as u64);
            epoch = prom_metric(&body, "cardano_node_metrics_epoch_int").map(|n| n as u64);
            density = prom_metric(&body, "cardano_node_metrics_density_real");
            peers = prom_metric(&body, "cardano_node_metrics_connectedPeers_int");
            if peers.unwrap_or(0.0) < 1.0 {
                reasons.push("no connected peers".into());
            }
        }
        Ok((st, _)) => reasons.push(format!("prometheus HTTP {st}")),
        Err(e) => reasons.push(format!("prometheus: {e}")),
    }

    let mut tip = Value::Null;
    if socket_ok {
        let mut args = vec!["query".into(), "tip".into(), "--socket-path".into(), socket.clone()];
        args.extend(cardano_network_args(params));
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        match run_cmd("cardano-cli", &refs, &[]) {
            Ok(out) => {
                tip = serde_json::from_str(out.trim()).unwrap_or(json!(out.trim()));
                if block_height.is_none() {
                    block_height = tip.get("block").and_then(|v| v.as_u64());
                }
                if slot.is_none() {
                    slot = tip.get("slot").and_then(|v| v.as_u64());
                }
                if epoch.is_none() {
                    epoch = tip.get("epoch").and_then(|v| v.as_u64());
                }
                sync_progress = tip.get("syncProgress").and_then(|v| match v {
                    Value::String(s) => s.parse::<f64>().ok(),
                    Value::Number(n) => n.as_f64(),
                    _ => None,
                });
                if let Some(s) = sync_progress {
                    if s < CARDANO_SYNC_OK {
                        reasons.push(format!("syncProgress {s}%"));
                    }
                }
            }
            Err(e) => reasons.push(format!("cardano-cli: {e}")),
        }
    } else {
        reasons.push(format!("socket missing: {socket}"));
    }

    let running = !procs.is_empty() || unit == Some(true);
    let mut status = "ok";
    if !running && !prom_ok {
        status = "fail";
        if reasons.is_empty() {
            reasons.push("cardano-node not running and metrics unreachable".into());
        }
    } else if reasons.iter().any(|r| r.contains("syncProgress")) {
        status = "warn";
    } else if !running {
        status = "warn";
        reasons.push("process not found".into());
    } else if !prom_ok {
        status = "warn";
    }
    if reasons.is_empty() {
        reasons.push(match block_height {
            Some(h) => format!("block {h}"),
            None => "ok".into(),
        });
    }

    wrap(
        "cardano_node",
        status,
        &reasons.join("; "),
        json!({
            "block_height": block_height,
            "slot": slot,
            "epoch": epoch,
            "sync_progress": sync_progress,
            "density": density,
            "connected_peers": peers,
            "prometheus_url": prom_url,
            "socket": socket,
            "socket_ok": socket_ok,
            "systemd_active": unit,
            "tip": tip,
            "processes": procs,
        }),
    )
}

fn parse_pg_row(s: &str) -> Option<(i64, i64)> {
    let line = s.lines().find(|l| !l.trim().is_empty())?.trim();
    let mut it = line.split(|c| c == '|' || c == '\t' || c == ',');
    let a = it.next()?.trim().parse().ok()?;
    let b = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    Some((a, b))
}

pub fn check_db_sync(params: &Value) -> Value {
    let procs = processes_matching(&["cardano-db-sync", "cardano-db-sync-extended"]);
    let unit = unit_active("cardano-db-sync.service");
    let creds = load_env_file(&param_str(params, "creds", "FNO_DB_CREDENTIALS", DEFAULT_CREDS));
    let database = param_str(
        params,
        "database",
        "PGDATABASE",
        creds.get("DB_NAME").map(|s| s.as_str()).unwrap_or("cexplorer"),
    );
    let host = param_str(
        params,
        "pghost",
        "PGHOST",
        creds.get("DB_HOST").map(|s| s.as_str()).unwrap_or("127.0.0.1"),
    );
    let port = param_str(
        params,
        "pgport",
        "PGPORT",
        creds.get("DB_PORT").map(|s| s.as_str()).unwrap_or("5432"),
    );
    let user = param_str(
        params,
        "pguser",
        "PGUSER",
        creds.get("DB_USER").map(|s| s.as_str()).unwrap_or("midnight"),
    );
    let pass = env::var("PGPASSWORD").ok().or_else(|| creds.get("DB_PASS").cloned());
    let lag_warn = param_u64(params, "lag_warn", "DBSYNC_LAG_WARN", DBSYNC_LAG_WARN as u64) as i64;
    let lag_fail = param_u64(params, "lag_fail", "DBSYNC_LAG_FAIL", DBSYNC_LAG_FAIL as u64) as i64;

    let mut envv = vec![
        ("PGDATABASE", database.clone()),
        ("PGHOST", host.clone()),
        ("PGPORT", port.clone()),
        ("PGUSER", user.clone()),
    ];
    if let Some(p) = pass {
        envv.push(("PGPASSWORD", p));
    }
    let sql = "SELECT COALESCE(MAX(block_no),0), COALESCE(MAX(slot_no),0) FROM block;";
    let pg = run_cmd("psql", &["-At", "-F", "|", "-c", sql], &envv);

    let mut db_block = None;
    let mut db_slot = None;
    let mut pg_err = None;
    match pg {
        Ok(out) => match parse_pg_row(&out) {
            Some((b, s)) => {
                db_block = Some(b);
                db_slot = Some(s);
            }
            None => pg_err = Some(format!("unexpected psql output: {}", out.trim())),
        },
        Err(e) => pg_err = Some(e),
    }

    let node = check_cardano_node(params);
    let node_block = node
        .get("block_height")
        .and_then(|v| v.as_u64())
        .map(|n| n as i64)
        .or_else(|| node.get("tip").and_then(|t| t.get("block")).and_then(|v| v.as_u64()).map(|n| n as i64));
    let lag = match (db_block, node_block) {
        (Some(d), Some(n)) => Some(n - d),
        _ => None,
    };

    let running = !procs.is_empty() || unit == Some(true);
    let mut status = "ok";
    let mut reason = match db_block {
        Some(h) => format!("block {h}"),
        None => "ok".into(),
    };
    if pg_err.is_some() && !running {
        status = "fail";
        reason = format!("db-sync not running; postgres: {}", pg_err.clone().unwrap_or_default());
    } else if pg_err.is_some() {
        status = "fail";
        reason = format!("postgres query failed: {}", pg_err.clone().unwrap_or_default());
    } else if !running {
        status = "warn";
        reason = "postgres reachable but cardano-db-sync not running".into();
    } else if let Some(l) = lag {
        if l >= lag_fail {
            status = "fail";
            reason = format!("db-sync lag {l} blocks (fail>={lag_fail})");
        } else if l >= lag_warn {
            status = "warn";
            reason = format!("db-sync lag {l} blocks (warn>={lag_warn})");
        } else if l < 0 {
            status = "ok";
            reason = format!("db-sync ahead of node by {} blocks", -l);
        }
    }

    wrap(
        "cardano_db_sync",
        status,
        &reason,
        json!({
            "block_height": db_block,
            "db_block": db_block,
            "db_slot": db_slot,
            "node_block": node_block,
            "lag_blocks": lag,
            "database": database, "pghost": host, "pgport": port, "pguser": user,
            "postgres_error": pg_err,
            "systemd_active": unit, "processes": procs,
        }),
    )
}
