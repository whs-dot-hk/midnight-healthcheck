# midnight-healthcheck

A healthcheck for a Midnight federated-node-operator host: `cardano-node`, `cardano-db-sync`
and `midnight-node`, plus the things that make them *correct* rather than merely running.

Speaks line-delimited JSON-RPC 2.0 on stdin/stdout, so it works over plain SSH with no agent
and no open port:

```bash
ssh node 'sudo healthcheck --once'
echo '{"jsonrpc":"2.0","id":1,"method":"check","params":{"name":"progress"}}' | ssh node sudo healthcheck
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
| `binaries` | Is each unit executing the binary that is on disk, or one replaced underneath it? |
| `secrets` | Is the validator key material present, and readable only by its owner? |
| `midnight`, `cardano_node`, `cardano_db_sync` | Process/unit liveness, RPC reachability, peers, db-sync lag. |
| `disk`, `memory`, `load`, `uptime` | Host context. Reported, never counted toward overall health. |

### `progress` needs two runs

It stores one observation (`/var/lib/midnight-healthcheck/state.json`, override with
`HEALTHCHECK_STATE`) and compares against it. The first run records a baseline and reaches no
verdict; so does any run less than 45 s after the previous one, which is reported rather than
guessed at. A timer every 5 minutes is the intended use, and a longer interval also gives a
steadier rate — a 60-second window is a noisy basis for an ETA, though it is perfectly good
for deciding whether anything moved at all.

### Deliberate non-alarms

* A node **catching up** is `ok`. `progress` owns the question of whether it is stuck.
* The **first-start index build** (the node builds indexes on `cexplorer` and opens no ports
  until done, which looks exactly like a dead node) is reported as starting, not failed.
* An **unreachable public RPC** is `warn`, not `fail`: not being able to check is a gap in
  knowledge, not evidence of a fault here.

## Running it

Needs root, for the journal, the Cardano socket, `psql`, and the key-material modes.

```bash
sudo healthcheck --once | jq .
sudo healthcheck --once | jq -r '.result | .status, (.checks[] | "\(.status)\t\(.name)\t\(.reason)")'
```

Every path and URL has a default matching the FNO layout, overridable by env var or by an RPC
param:

| env | default |
|---|---|
| `MIDNIGHT_RPC_URL` | probed: `http://127.0.0.1:9944`, then `:9933` |
| `MIDNIGHT_NETWORK_RPC_URL` | `https://rpc.preprod.midnight.network` |
| `CARDANO_NODE_SOCKET_PATH` | `/data/cardano/db/node.socket` |
| `CARDANO_CLI` | first of `~midnight/.local/bin`, `/usr/local/bin`, `/usr/bin` |
| `CARDANO_TESTNET_MAGIC` | `1` (preprod) |
| `MIDNIGHT_NODE_DATA` | `/data/midnight_node` |
| `MIDNIGHT_CHAIN_ID` | `midnight_preprod` |
| `HEALTHCHECK_STATE` | `/var/lib/midnight-healthcheck/state.json` |

`sudo` resets `PATH` to its `secure_path`, which excludes the service user's `~/.local/bin`
where the installer puts `cardano-cli` — hence the absolute-path resolution rather than a
lookup.

## On a timer

```bash
sudo install -m0755 healthcheck /usr/local/bin/healthcheck
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
