# Agent CPU benchmark: mirrord-agent vs mirrord-agent-ractor

Measures the CPU cost of moving outgoing TCP traffic through each agent,
expressed as **agent CPU milliseconds per MiB of payload relayed**.

## Topology

Everything runs in a kind cluster:

* `echo-server` - trivial multi-threaded Rust TCP/UDP echo (`echo_server` bin),
  reachable through a ClusterIP service;
* `agent-original` - stock `mirrord-agent` in targetless mode, NodePort 30061;
* `agent-ractor` - `mirrord-agent-ractor` in targetless mode, NodePort 30062.

The `bench_client` bin runs on the host and speaks raw mirrord-protocol to an
agent through its NodePort. It opens N outgoing connections to the echo service
through the agent, pumps fixed-size write chunks into them, and drains the
echoes; every payload byte therefore crosses the agent four times on two sockets
(client->agent->echo, echo->agent->client).

The client caps sent-but-not-yet-echoed bytes at a window (`--window-kib`,
default 256KiB) kept below the agents' 512KiB per-direction memory budgets.
This is not just politeness: an unpaced client deadlocks mirrord-agent under
full bidirectional saturation (its single client loop blocks acquiring the
client->peer budget and stops draining peer->client data, which is what frees
the peer->client budget the echoes need), stalling connections until the 30s
write timeout kills them. mirrord-agent-ractor is structurally immune - the
actor that writes to the client keeps running while the dispatcher is blocked -
so the window exists purely to give both agents the same completable workload.

Agent CPU is self-sampled: both agents run with
`MIRRORD_AGENT_CPU_SAMPLE_MS=100`, which makes them log a
`CPUSAMPLE <epoch_ms> <cumulative cpu ticks>` series from `/proc/self/stat`
(see `cpu_sample.rs`; the identical module is planted in both agents). The
client stamps each run's start/end, and `run_bench.sh` interpolates the series
at those points - nothing execs into the pods during measurement.

## Running

```bash
# Build everything (host side).
cargo build -p mirrord-agent-ractor --release --bins
cargo build -p mirrord-agent --release --target x86_64-unknown-linux-gnu

# Cluster + deployment.
kind create cluster --name bench
staging=$(mktemp -d)
cp target/x86_64-unknown-linux-gnu/release/mirrord-agent \
   target/release/mirrord-agent-ractor \
   target/release/echo_server "$staging"
docker build -t mirrord-bench:latest -f mirrord/agent-ractor/bench/Dockerfile "$staging"
kind load docker-image mirrord-bench:latest --name bench
kubectl apply -f mirrord/agent-ractor/bench/k8s.yaml
kubectl wait --for=condition=Ready pod --all --timeout 120s

# Functional check of both agents through the cluster (optional but recommended).
node_ip=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')
echo_ip=$(kubectl get svc echo-server -o jsonpath='{.spec.clusterIP}')
./target/release/verify_client "$node_ip:30062" "$echo_ip:7777" "$echo_ip:7777" --unordered-dns
./target/release/verify_client "$node_ip:30061" "$echo_ip:7777" "$echo_ip:7777" --unordered-dns --full-agent

# The benchmark itself.
./mirrord/agent-ractor/bench/run_bench.sh
```

The matrix covers 64KiB chunks (bulk relaying) and 4KiB chunks (per-message
overhead, where an actor framework's mailbox hops cost the most), with 4
connections and a single-connection case. No CPU limits are set on any pod -
throttling would distort the measurement.

## Hardened-sandbox quirk

Some sandboxed hosts (custom kernels) refuse writes of *negative*
`/proc/*/oom_score_adj` even with `CAP_SYS_RESOURCE`, while kubelet hard-codes
`-998` for sandboxes - every pod then fails with runc's
`can't get final child's PID from pipe: EOF`. On such hosts, wrap runc inside
the kind node to clamp the value before `create`:

```bash
docker exec bench-control-plane bash -c '
mv /usr/local/sbin/runc /usr/local/sbin/runc.real
cat > /usr/local/sbin/runc <<"EOF"
#!/bin/bash
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  if [[ "${args[i]}" == "--bundle" || "${args[i]}" == "-b" ]]; then
    b="${args[i+1]}"
    [[ -f "$b/config.json" ]] && sed -i "s/\"oomScoreAdj\":-[0-9]*/\"oomScoreAdj\":0/g" "$b/config.json"
  fi
done
exec /usr/local/sbin/runc.real "$@"
EOF
chmod +x /usr/local/sbin/runc'
```

Not needed on normal Linux hosts.

## Results (2026-08-28, 4-core sandbox VM, kind v1.29.2)

Full data in `results-2026-08-28.csv` (3 reps per cell; medians below).
`cpu_ms_per_mib` is agent CPU milliseconds per MiB of payload pushed through.

| workload          | original | ractor | ractor vs original |
|-------------------|----------|--------|--------------------|
| 64KiB chunks, 4 conns | 1.60 ms/MiB @ 599 MiB/s | 1.24 ms/MiB @ 738 MiB/s | **-23% CPU**, +23% throughput |
| 4KiB chunks, 4 conns  | 2.77 ms/MiB @ 350 MiB/s | 3.46 ms/MiB @ 278 MiB/s | **+25% CPU**, -21% throughput |
| 64KiB chunks, 1 conn  | 1.38 ms/MiB @ 574 MiB/s | 1.19 ms/MiB @ 724 MiB/s | **-14% CPU**, +26% throughput |

Reading: on bulk traffic the actor version is *cheaper* - one mailbox hop
replaces the original's throttle/buffer/SelectAll wrapper stack, and
`write_all` replaces the vectored sink machinery. On small chunks the
per-message actor tax (3 mailbox hops client->peer, each with a boxed message
and an unbounded-channel node, plus a budget reservation per chunk) outweighs
those savings: at 4KiB every MiB costs 256 messages each way, ~2.7us more CPU
per relayed chunk than the original path. Both agents saturate one core in the
4KiB workload, so CPU-per-MiB and throughput are two views of the same number.

Benchmarking also surfaced two pre-existing mirrord-agent issues, both fixed
on this branch:

1. **Data corruption on partial vectored writes** (`IoVecThrottledSink`): the
   buffered chunk was replaced with the already-written prefix instead of the
   remainder, resending up to `written` bytes and dropping the tail.
2. **Budget deadlock under bidirectional saturation** (not fixed, worked
   around by client pacing): the client loop blocks on the client->peer budget
   and stops draining peer->client data, which is what frees the peer->client
   budget; connections stall until the 30s write timeout kills them. The actor
   topology is structurally immune.
