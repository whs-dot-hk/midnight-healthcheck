//! JSON-RPC 2.0 healthcheck over stdin/stdout (SSH-friendly).
//!
//!   ssh user@host midnight-healthcheck
//!   echo '{"jsonrpc":"2.0","id":1,"method":"health"}' | ssh user@host midnight-healthcheck
//!
//! Each check is just: is this component healthy? (`ok` / `warn` / `fail`)
//!
//! | check            | what it does |
//! |------------------|--------------|
//! | disk/memory/load/uptime | host info (always reported; not used for overall health) |
//! | midnight         | Midnight Substrate RPC (`MIDNIGHT_RPC_URL`, default http://127.0.0.1:9944) |
//! | cardano_node     | systemd/process + Prometheus + optional `cardano-cli query tip` |
//! | cardano_db_sync  | process + postgres `max(block_no)` vs node tip |
//! | progress         | is it *moving*? deltas against the previous run — the check that separates "catching up" from "wedged" |
//! | chain_identity   | genesis + version against the live network, not against a pin |
//! | binaries         | running image vs the binary on disk (`enable --now` does not restart) |
//! | secrets          | key material present and owner-only |
//!
//! Layout matches https://github.com/whs-dot-hk/midnight-installer
//! (`/data/cardano/db/node.socket`, `/data/postgresql/fno-db-credentials.env`, preprod).

mod nodes;
mod trend;
mod verify;

use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const INFO_CHECKS: &[&str] = &["disk", "memory", "load", "uptime"];
const CHECKS: &[&str] = &[
    "disk",
    "memory",
    "load",
    "uptime",
    "midnight",
    "cardano_node",
    "cardano_db_sync",
    "progress",
    "chain_identity",
    "binaries",
    "secrets",
];

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn utc_now() -> String {
    let t = now_unix();
    rfc3339(t.floor() as i64, ((t - t.floor()) * 1e9) as u32)
}

fn rfc3339(secs: i64, nanos: u32) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    let h = tod / 3600;
    let min = (tod % 3600) / 60;
    let s = tod % 60;
    if nanos == 0 {
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}+00:00")
    } else {
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{nanos:09}+00:00")
    }
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

fn read_proc(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn mem_get(mi: &[(String, u64)], k: &str) -> u64 {
    mi.iter().find(|(n, _)| n == k).map(|(_, v)| *v).unwrap_or(0)
}

fn meminfo() -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for line in read_proc("/proc/meminfo").unwrap_or_default().lines() {
        let mut p = line.split_whitespace();
        let key = p.next().unwrap_or("").trim_end_matches(':');
        if let Some(v) = p.next().and_then(|x| x.parse::<u64>().ok()) {
            out.push((key.to_string(), v * 1024));
        }
    }
    out
}

fn loadavg() -> Option<(f64, f64, f64)> {
    let raw = read_proc("/proc/loadavg")?;
    let mut it = raw.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn cpu_count() -> u32 {
    read_proc("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count().max(1) as u32)
        .unwrap_or(1)
}

fn disk_usage(path: &str) -> io::Result<(u64, u64, u64)> {
    use std::ffi::CString;
    let c = CString::new(path).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        let b = st.f_frsize as u64;
        let total = st.f_blocks as u64 * b;
        let free = st.f_bavail as u64 * b;
        let used = total.saturating_sub(st.f_bfree as u64 * b);
        Ok((total, used, free))
    }
}

fn cstr(buf: &[libc::c_char]) -> String {
    let b: Vec<u8> = buf.iter().map(|c| *c as u8).take_while(|&x| x != 0).collect();
    String::from_utf8_lossy(&b).into_owned()
}

fn hostname() -> String {
    let mut buf = [0 as libc::c_char; 256];
    unsafe {
        if libc::gethostname(buf.as_mut_ptr(), buf.len()) == 0 {
            return cstr(&buf);
        }
    }
    "unknown".into()
}

fn uname() -> (String, String, String) {
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) == 0 {
            return (cstr(&u.sysname), cstr(&u.release), cstr(&u.machine));
        }
    }
    ("unknown".into(), "unknown".into(), "unknown".into())
}

fn worst(ss: impl IntoIterator<Item = String>) -> String {
    let mut rank = 0;
    let mut out = "ok".to_string();
    for s in ss {
        let r = match s.as_str() {
            "fail" => 2,
            "warn" => 1,
            _ => 0,
        };
        if r > rank {
            rank = r;
            out = s;
        }
    }
    out
}

pub(crate) fn wrap(name: &str, status: &str, reason: &str, extra: Value) -> Value {
    let info = INFO_CHECKS.contains(&name);
    let mut o = json!({
        "name": name,
        "kind": if info { "info" } else { "health" },
        "status": if info { "ok" } else { status },
        "healthy": info || status != "fail",
        "reason": reason,
    });
    if let Value::Object(map) = extra {
        if let Some(obj) = o.as_object_mut() {
            for (k, v) in map {
                obj.insert(k, v);
            }
        }
    }
    o
}

fn check_disk(params: &Value) -> Value {
    let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("/");
    match disk_usage(path) {
        Ok((total, used, free)) => {
            let pct = if total == 0 { 0.0 } else { used as f64 / total as f64 * 100.0 };
            let pct = (pct * 100.0).round() / 100.0;
            wrap(
                "disk",
                "ok",
                &format!("{pct}% used"),
                json!({"path": path, "total_bytes": total, "used_bytes": used, "free_bytes": free, "used_percent": pct}),
            )
        }
        Err(e) => wrap("disk", "ok", &e.to_string(), json!({})),
    }
}

fn check_memory(_: &Value) -> Value {
    let mi = meminfo();
    let total = mem_get(&mi, "MemTotal");
    let avail = {
        let a = mem_get(&mi, "MemAvailable");
        if a == 0 { mem_get(&mi, "MemFree") } else { a }
    };
    let used = total.saturating_sub(avail);
    let pct = if total == 0 { 0.0 } else { used as f64 / total as f64 * 100.0 };
    let pct = (pct * 100.0).round() / 100.0;
    let swap_total = mem_get(&mi, "SwapTotal");
    wrap(
        "memory",
        "ok",
        &format!("{pct}% used"),
        json!({
            "total_bytes": total, "used_bytes": used, "available_bytes": avail,
            "used_percent": pct, "swap_total_bytes": swap_total,
            "swap_used_bytes": swap_total.saturating_sub(mem_get(&mi, "SwapFree")),
        }),
    )
}

fn check_load(_: &Value) -> Value {
    let ncpu = cpu_count();
    match loadavg() {
        Some((a, b, c)) => {
            let per = if ncpu == 0 { a } else { a / ncpu as f64 };
            wrap(
                "load",
                "ok",
                &format!("load1/cpu {per:.2}"),
                json!({"load_1": a, "load_5": b, "load_15": c, "cpus": ncpu, "load_1_per_cpu": (per * 1000.0).round() / 1000.0}),
            )
        }
        None => wrap("load", "ok", "loadavg unavailable", json!({})),
    }
}

fn check_uptime(_: &Value) -> Value {
    let up = read_proc("/proc/uptime")
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);
    wrap(
        "uptime",
        "ok",
        "ok",
        json!({"uptime_seconds": up as i64, "boot_time": rfc3339((now_unix() - up).floor() as i64, 0)}),
    )
}

fn run_check(name: &str, params: &Value) -> Value {
    match name {
        "disk" => check_disk(params),
        "memory" => check_memory(params),
        "load" => check_load(params),
        "uptime" => check_uptime(params),
        "midnight" => nodes::check_midnight(params),
        "cardano_node" => nodes::check_cardano_node(params),
        "cardano_db_sync" => nodes::check_db_sync(params),
        "progress" => trend::check(params),
        "chain_identity" => verify::check_chain_identity(params),
        "binaries" => verify::check_binaries(params),
        "secrets" => verify::check_secrets(params),
        _ => wrap(name, "fail", "unknown check", json!({})),
    }
}

fn run_all(params: &Value) -> Value {
    let names: Vec<String> = params
        .get("checks")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_else(|| CHECKS.iter().map(|s| (*s).to_string()).collect());
    let checks: Vec<Value> = names.iter().map(|n| run_check(n, params)).collect();
    let statuses: Vec<String> = checks
        .iter()
        .filter(|c| c.get("kind").and_then(|k| k.as_str()) != Some("info"))
        .map(|c| c.get("status").and_then(|s| s.as_str()).unwrap_or("fail").to_string())
        .collect();
    let overall = if statuses.is_empty() { "ok".into() } else { worst(statuses) };
    json!({
        "status": overall,
        "healthy": overall != "fail",
        "checked_at": utc_now(),
        "checks": checks,
    })
}

fn rpc_error(code: i32, message: &str, id: Value, data: Option<Value>) -> Value {
    let mut err = json!({"code": code, "message": message});
    if let Some(d) = data {
        err["data"] = d;
    }
    json!({"jsonrpc": "2.0", "error": err, "id": id})
}

fn rpc_result(result: Value, id: Value) -> Value {
    json!({"jsonrpc": "2.0", "result": result, "id": id})
}

fn as_params(params: Option<&Value>) -> Result<Value, String> {
    match params {
        None | Some(Value::Null) => Ok(json!({})),
        Some(Value::Object(_)) => Ok(params.unwrap().clone()),
        Some(Value::Array(a)) if a.first().map(|x| x.is_object()).unwrap_or(false) => Ok(a[0].clone()),
        _ => Err("params must be an object".into()),
    }
}

fn handle_request(req: &Value) -> Option<Value> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let note = req.get("id").is_none();
    if req.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Some(rpc_error(-32600, "Invalid Request: jsonrpc must be '2.0'", id, None));
    }
    let method = match req.get("method").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return Some(rpc_error(-32600, "Invalid Request: method required", id, None)),
    };
    let params = match as_params(req.get("params")) {
        Ok(p) => p,
        Err(e) => return Some(rpc_error(-32602, &e, id, None)),
    };
    let result = match method {
        "ping" => json!({"pong": true, "ts": utc_now()}),
        "health" | "status" => run_all(&params),
        "checks" => json!({"checks": CHECKS}),
        "check" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if !CHECKS.contains(&name) {
                return Some(rpc_error(-32602, "unknown check", id, Some(json!({"known": CHECKS}))));
            }
            run_check(name, &params)
        }
        "sysinfo" => {
            let (os, rel, mach) = uname();
            json!({
                "hostname": hostname(), "os": os, "release": rel, "machine": mach,
                "pid": process::id(), "healthcheck_version": VERSION,
            })
        }
        "help" => json!({
            "methods": {
                "ping": "liveness",
                "health": "run all checks (alias: status)",
                "checks": "list check names",
                "check": "run one check; params: {name, url?, socket?, ...}",
                "sysinfo": "host info",
                "help": "this message",
            },
            "checks": {
                "disk": "info: filesystem usage (does not affect overall health)",
                "memory": "info: RAM / swap",
                "load": "info: load average",
                "uptime": "info: host uptime",
                "midnight": "Midnight Substrate RPC (MIDNIGHT_RPC_URL, default http://127.0.0.1:9944)",
                "cardano_node": "cardano-node process + Prometheus + optional cardano-cli tip",
                "cardano_db_sync": "cardano-db-sync process + postgres block tip vs node",
                "progress": "is each component actually advancing? compares against the previous run (state file); a component behind the tip and not moving is a failure",
                "chain_identity": "local genesis and version vs the live network (MIDNIGHT_NETWORK_RPC_URL)",
                "binaries": "is each unit running the binary that is on disk, or one replaced under it?",
                "secrets": "validator key material: present, and readable only by its owner",
            },
            "transport": "line-delimited JSON-RPC 2.0 on stdin/stdout",
        }),
        _ => return Some(rpc_error(-32601, &format!("Method not found: {method}"), id, None)),
    };
    if note { None } else { Some(rpc_result(result, id)) }
}

fn dispatch(obj: Value) -> Option<Value> {
    match obj {
        Value::Array(items) => {
            if items.is_empty() {
                return Some(rpc_error(-32600, "Invalid Request: empty batch", Value::Null, None));
            }
            let mut out = Vec::new();
            for item in items {
                if item.is_object() {
                    if let Some(r) = handle_request(&item) {
                        out.push(r);
                    }
                } else {
                    out.push(rpc_error(-32600, "Invalid Request", Value::Null, None));
                }
            }
            if out.is_empty() { None } else { Some(Value::Array(out)) }
        }
        Value::Object(_) => handle_request(&obj),
        _ => Some(rpc_error(-32600, "Invalid Request", Value::Null, None)),
    }
}

fn write_response(resp: Option<Value>) {
    if let Some(v) = resp {
        let mut o = io::stdout().lock();
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    }
}

fn serve() -> i32 {
    for line in io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "." | "quit" | "exit") {
            break;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => write_response(dispatch(v)),
            Err(e) => write_response(Some(rpc_error(-32700, "Parse error", Value::Null, Some(json!(e.to_string()))))),
        }
    }
    0
}

fn help() {
    print!(
        "midnight-healthcheck — JSON-RPC 2.0 over stdio\n\
         \n\
         Usage: midnight-healthcheck [--once] [--help]\n\
         SSH:   ssh user@host midnight-healthcheck\n\
                echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"health\"}}' | ssh user@host midnight-healthcheck\n\
         \n\
         Methods: ping, health, checks, check, sysinfo, help\n\
         Checks:  disk, memory, load, uptime, midnight, cardano_node, cardano_db_sync\n\
         \n\
         Env: MIDNIGHT_RPC_URL\n\
              CARDANO_NODE_SOCKET_PATH (default /data/cardano/db/node.socket)\n\
              CARDANO_PROMETHEUS_URL (default http://127.0.0.1:12798/metrics)\n\
              CARDANO_NETWORK (default preprod) CARDANO_TESTNET_MAGIC (default 1)\n\
              PGDATABASE/PGHOST/PGPORT/PGUSER  or  /data/postgresql/fno-db-credentials.env\n"
    );
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let code = if args.iter().any(|a| a == "-h" || a == "--help") {
        help();
        0
    } else if args.iter().any(|a| a == "--once" || a == "-1") {
        let r = run_all(&json!({}));
        write_response(Some(rpc_result(r.clone(), json!(1))));
        if r.get("status").and_then(|s| s.as_str()) == Some("fail") { 2 } else { 0 }
    } else {
        serve()
    };
    process::exit(code);
}
