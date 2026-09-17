# midnight-healthcheck

A healthcheck for a Midnight federated-node-operator host: `cardano-node`, `cardano-db-sync`
and `midnight-node`, plus the things that make them *correct* rather than merely running.

Speaks line-delimited JSON-RPC 2.0 on stdin/stdout, so it works over plain SSH with no agent
and no open port:

```bash
ssh node 'sudo midnight-healthcheck --once'
echo '{"jsonrpc":"2.0","id":1,"method":"check","params":{"name":"progress"}}' | ssh node sudo midnight-healthcheck
```

Exit code with `--once`: `0` healthy, `2` something failed.

## The point: no false alarms, and no false calm

A healthcheck earns its keep only if a red line means "go look now". Two ways to lose that,
and this tool is built around avoiding both.

**False alarms.** An initial Midnight sync runs for *days*. A check that treats "behind the
tip" as a failure is red for three days while nothing is wrong, and by day two nobody reads
it. So `midnight` reports a node that is catching up as `ok`, and says so plainly. Likewise
an absent Prometheus endpoint is not flagged when the tip was read directly, and the RPC port
is probed (9944, then 9933) rather than assumed — a healthy node reported as down because the
port was guessed is the most expensive noise there is.

**False calm.** The harder half. These all read as healthy locally:

| what happened | what a naive check saw | what catches it |
|---|---|---|
| `cardano-node` too old for the chain's protocol version — relay wedged permanently | `query tip` returning well-formed JSON, `syncProgress 99.96` | `progress` — the block number stops changing |
| node booted from a chain spec that is not the live chain | `Role: AUTHORITY`, peers connected, syncing | `chain_identity` — block 0 vs the network |
| version bumped, `systemctl enable --now` did not restart an already-active unit | new binary on disk, old one still running | `binaries` — `/proc/<pid>/exe` marked `(deleted)` |
| a pinned release far behind what the network runs | node runs fine, never joins | `chain_identity` — `system_version` vs the network |

## Checks

| check | what it answers |
|---|---|
| `progress` | **Is it moving?** Compares block heights against the previous run. Behind *and* advancing → `ok`, with a rate and a rough ETA. Behind and *not* advancing → `fail`. |
| `chain_identity` | Is this the same chain, and roughly the same release, as the live network? Genesis mismatch is `fail`; a version skew is `warn`. |
| `binaries` | Is each unit executing the binary that is on disk, or one replaced underneath it? `warn` if no checked unit is running at all, rather than a vacuous all-clear. |
| `secrets` | Is the validator key material present under `SECRET_ROOT` (`/secret`), and readable only by its owner? `.env` and the `.seed` files are written by the validator stage — hours or days after the keys — so their absence before then is not a finding. |
| `midnight`, `cardano_node`, `cardano_db_sync` | Process/unit liveness, RPC reachability, peers, db-sync lag. |
| `disk`, `memory`, `load`, `uptime` | Host context. Reported, never counted toward overall health. |

### `progress` needs two runs

It stores one observation per component (`/var/lib/midnight-healthcheck/state.json`, override
with `HEALTHCHECK_STATE`; falls back to `~/.local/state/`, never `/tmp`) and compares against
it. Each component has its own slot with its own timestamp, so `midnight` — which reaches the
same verdict for itself, so that a monitor polling only that check still gets stall detection —
cannot disturb `progress`'s baseline or vice versa.

The rules that keep the verdict honest:

* The first run records a baseline and reaches no verdict.
* A run less than 45 s after the baseline reaches no verdict either — **and keeps the old
  baseline**, so polling faster than the interval cannot starve detection.
* A reading that could not be taken is reported as exactly that. It is never rounded to "at
  the tip", and it does not overwrite a good baseline.
* A block number that went **backwards** is a restart or a rollback (routine for db-sync, and
  the prescribed remedy for a genesis mismatch) — reported as `warn` with the baseline reset,
  not as a stall.

A timer every 5 minutes is the intended use, and a longer interval also gives a steadier rate —
a 60-second window is a noisy basis for an ETA, though perfectly good for deciding whether
anything moved at all.

### Deliberate non-alarms

* A node **catching up** is `ok`. `progress` owns the question of whether it is stuck.
* The **first-start index build** (the node builds indexes on `cexplorer` and opens no ports
  until done, which looks exactly like a dead node) is reported as starting, not failed.
* An **unreachable public RPC** is `warn`, not `fail`: not being able to check is a gap in
  knowledge, not evidence of a fault here.
* The first-start index-build allowance applies **only** to the indexes `midnight-node`
  itself creates, and only while its unit is active. db-sync's own index work, which can run
  for hours, does not excuse a node that is actually down.
* An **absent Prometheus endpoint** on the relay does not change the status when the tip was
  read directly — but the reason says `peer count unknown`, because that is what it means.

## Running it

Needs root, for the journal, the Cardano socket, `psql`, and the key-material modes.

```bash
sudo midnight-healthcheck --once | jq .
sudo midnight-healthcheck --once | jq -r '.result | .status, (.checks[] | "\(.status)\t\(.name)\t\(.reason)")'
```

Every path and URL has a default matching the FNO layout, overridable by env var or by an RPC
param:

| env | default |
|---|---|
| `MIDNIGHT_RPC_URL` | probed: `http://127.0.0.1:9944`, then `:9933` |
| `MIDNIGHT_NETWORK_RPC_URL` | `https://rpc.preprod.midnight.network` |
| `CARDANO_NODE_SOCKET_PATH` | `/data/cardano/db/node.socket` |
| `CARDANO_USER` | the `User=` of `cardano-node.service` |
| `CARDANO_CLI` | first of that user's `~/.local/bin/cardano-cli`, `/usr/local/bin`, `/usr/bin` |
| `CARDANO_NETWORK` / `CARDANO_TESTNET_MAGIC` | `preprod` / `1` |
| `SECRET_ROOT` | `/secret` |
| `MIDNIGHT_NODE_DATA` | `/data/midnight_node` |
| `MIDNIGHT_CHAIN_ID` | `midnight_preprod` |
| `HEALTHCHECK_STATE` | `/var/lib/midnight-healthcheck/state.json` |

`sudo` resets `PATH` to its `secure_path`, which excludes the service user's `~/.local/bin`
where the installer puts `cardano-cli` — hence the absolute-path resolution rather than a
lookup. The service user itself is read from the unit rather than assumed, because the setup
script defaults it to whoever ran it.

Every connection has a connect timeout as well as an I/O timeout, and the RPC port is probed
once per process, so a hung or firewalled endpoint costs one timeout, not one per check.

## Responses

Line-delimited JSON-RPC 2.0: one request per line in, one response per line out. The examples
below are real output with host-identifying values replaced by placeholders — block heights,
hashes and hostnames from your own node will differ.

Every response is a standard envelope. Errors use the usual codes:

```json
{"jsonrpc":"2.0","id":2,"result":{"checks":["disk","memory","load","uptime","midnight",
 "cardano_node","cardano_db_sync","progress","chain_identity","binaries","secrets"]}}

{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"Method not found: nope"}}
```

`health` runs every check and wraps the results:

```json
{"jsonrpc":"2.0","id":5,"result":{
  "status":"ok","healthy":true,"checked_at":"<rfc3339>","checks":[ … ]}}
```

Every check body carries the same four fields — `name`, `status` (`ok` / `warn` / `fail`),
`healthy` (true unless `fail`), and `reason` — then adds its own. `chain_identity` reports
**both sides of every comparison**, so the output shows why it passed, not just that it did:

```json
{"name":"chain_identity","status":"ok","healthy":true,"kind":"health",
 "reason":"on the same chain as https://rpc.preprod.midnight.network (genesis 0xdf83…361b, 1.0.2-<build>)",
 "local_url":"http://127.0.0.1:9933",
 "network_url":"https://rpc.preprod.midnight.network",
 "local_genesis":"0xdf83…361b","network_genesis":"0xdf83…361b",
 "local_version":"1.0.2-<build>","network_version":"1.0.2-<build>"}
```

`progress` nests one object per component, each with the machine-readable verdict beside the
prose. Note `advancing` is **three-state** — `true` moving, `false` stalled, `null` not yet
determined — and is never collapsed to a boolean, because "I could not tell" and "it is not
moving" are different answers:

```json
{"name":"progress","status":"ok","healthy":true,
 "state_file":"/var/lib/midnight-healthcheck/state.json",
 "reason":"cardano-node: at the tip (block 5182990); midnight-node: behind at block 528749; no verdict yet (0s since baseline, need 45s)",
 "components":{
   "cardano_node":  {"status":"ok","block":5182990,"at_tip":true,"advancing":true,
                     "detail":"cardano-node: at the tip (block 5182990)"},
   "midnight_node": {"status":"ok","block":528749,"at_tip":false,"advancing":null,
                     "interval_secs":0.26,
                     "detail":"midnight-node: behind at block 528749; no verdict yet (0s since baseline, need 45s)"}}}
```

A stall — the failure the tool exists for. `previous_block` and `interval_secs` are included
so an alert can show its own evidence rather than asserting:

```json
{"name":"progress","status":"fail","healthy":false,
 "reason":"cardano-node: STALLED at block 100 — no progress in 300s while still behind the tip",
 "components":{
   "cardano_node":{"status":"fail","block":100,"previous_block":100,
                   "at_tip":false,"advancing":false,"interval_secs":300.19,
                   "detail":"cardano-node: STALLED at block 100 — no progress in 300s while still behind the tip"}}}
```

### Scripting against it

```bash
# one verdict for the whole host
sudo midnight-healthcheck --once | jq -r '.result.status'

# only what needs attention
sudo midnight-healthcheck --once | jq -r '.result.checks[] | select(.status != "ok") | "\(.status)\t\(.name)\t\(.reason)"'

# is the node actually moving? true / false / null
echo '{"jsonrpc":"2.0","id":1,"method":"check","params":{"name":"progress"}}' |
  sudo midnight-healthcheck | jq '.result.components.midnight_node.advancing'
```

`--once` mirrors the verdict in its exit code — `0` healthy, `2` something failed — so cron and
the systemd unit need no JSON parsing at all.

## On a timer

```bash
sudo install -m0755 target/release/midnight-healthcheck /usr/local/bin/midnight-healthcheck
sudo install -m0644 systemd/midnight-healthcheck.service systemd/midnight-healthcheck.timer /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now midnight-healthcheck.timer

journalctl -u midnight-healthcheck -f          # results
systemctl list-timers midnight-healthcheck     # schedule
```

Five minutes is the default interval: frequent enough that a stall is caught quickly, far
enough apart that the delta is real progress rather than jitter.

## Layout

Paths follow [midnight-installer](https://github.com/whs-dot-hk/midnight-installer) and the
interactive FNO setup script: `/data/cardano`, `/data/postgresql`, `/data/midnight_node`,
services `cardano-node`, `cardano-db-sync`, `midnight-node`, preprod.
