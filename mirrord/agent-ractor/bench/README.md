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
through the agent, saturates them with fixed-size write chunks, and drains the
echoes; every payload byte therefore crosses the agent four times on two sockets
(client->agent->echo, echo->agent->client).

Agent CPU is sampled around each run from `/proc/1/stat` (utime+stime of the
agent process, so exec probes and the pause container are excluded), with
cgroup v2 `cpu.stat` as a cross-check.

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
