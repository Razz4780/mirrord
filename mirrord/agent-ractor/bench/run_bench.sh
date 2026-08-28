#!/usr/bin/env bash
# CPU-cost benchmark: original mirrord-agent vs mirrord-agent-ractor.
#
# Prerequisites (see bench/README.md):
#   * a kind cluster with bench/k8s.yaml applied (image mirrord-bench:latest loaded),
#   * bench_client built on the host (cargo build -p mirrord-agent-ractor --release --bins).
#
# For every (agent, chunk size, connection count) combination this script runs
# the spam client REPS times and reports, per run, the agent's CPU time consumed
# per MiB pushed through it.
#
# CPU time comes from the agent itself: both agents run with
# MIRRORD_AGENT_CPU_SAMPLE_MS=100, making them log a `CPUSAMPLE <epoch_ms>
# <cumulative cpu ticks>` series (see cpu_sample.rs). The client prints its
# start/end timestamps, and this script interpolates the series at those points.
# The host and the kind node share a clock, so the timestamps line up.

set -euo pipefail

BENCH_CLIENT="${BENCH_CLIENT:-./target/release/bench_client}"
NODE_IP="$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')"
ECHO_IP="$(kubectl get svc echo-server -o jsonpath='{.spec.clusterIP}')"
REPS="${REPS:-3}"
RESULTS="${RESULTS:-bench_results.csv}"

declare -A AGENT_PORTS=(
  [original]=30061
  [ractor]=30062
)
declare -A AGENT_PODS=(
  [original]=agent-original
  [ractor]=agent-ractor
)

# cpu_seconds <pod> <start_ms> <end_ms>: CPU seconds the agent burned in the
# window, from its self-sampled CPUSAMPLE log lines (linear interpolation at the
# window edges; ticks are USER_HZ = 100).
cpu_seconds() {
  kubectl logs "$1" --tail 100000 | grep ^CPUSAMPLE | python3 -c '
import sys
start, end = float(sys.argv[1]), float(sys.argv[2])
samples = []
for line in sys.stdin:
    _, ms, ticks = line.split()
    samples.append((float(ms), int(ticks)))

def at(t):
    prev = next_ = None
    for s in samples:
        if s[0] <= t:
            prev = s
        elif next_ is None:
            next_ = s
            break
    if prev is None:
        return next_[1]
    if next_ is None or next_[0] == prev[0]:
        return prev[1]
    frac = (t - prev[0]) / (next_[0] - prev[0])
    return prev[1] + frac * (next_[1] - prev[1])

print(f"{(at(end) - at(start)) / 100:.2f}")
' "$2" "$3"
}

echo "node=$NODE_IP echo=$ECHO_IP client=$BENCH_CLIENT reps=$REPS"
echo "agent,run,chunk_kib,conns,sent_mib,wall_s,throughput_mib_s,cpu_s,cpu_ms_per_mib" | tee "$RESULTS"

# Matrix: 64KiB chunks measure bulk relaying, 4KiB chunks stress the
# per-message machinery, which is where an actor framework's overhead lives.
for agent in original ractor; do
  pod="${AGENT_PODS[$agent]}"
  port="${AGENT_PORTS[$agent]}"

  # Warmup: settles allocators, TCP windows and the page cache.
  "$BENCH_CLIENT" --agent "$NODE_IP:$port" --target "$ECHO_IP:7777" \
    --total-mib 256 --chunk-kib 64 --conns 4 > /dev/null

  for spec in "64 4" "4 4" "64 1"; do
    read -r chunk conns <<< "$spec"
    total_mib=$(( chunk >= 64 ? 2048 : 512 ))
    for rep in $(seq 1 "$REPS"); do
      out="$("$BENCH_CLIENT" --agent "$NODE_IP:$port" --target "$ECHO_IP:7777" \
        --total-mib "$total_mib" --chunk-kib "$chunk" --conns "$conns" | grep ^RESULT)"

      sent_mib="$(sed -n 's/.*sent_mib=\([0-9.]*\).*/\1/p' <<< "$out")"
      wall_s="$(sed -n 's/.*wall_s=\([0-9.]*\).*/\1/p' <<< "$out")"
      tput="$(sed -n 's/.*throughput_mib_s=\([0-9.]*\).*/\1/p' <<< "$out")"
      start_ms="$(sed -n 's/.*start_ms=\([0-9]*\).*/\1/p' <<< "$out")"
      end_ms="$(sed -n 's/.*end_ms=\([0-9]*\).*/\1/p' <<< "$out")"

      # Let the sampler write a data point past the window end.
      sleep 0.5
      cpu_s="$(cpu_seconds "$pod" "$start_ms" "$end_ms")"
      cpu_ms_per_mib="$(awk -v c="$cpu_s" -v m="$sent_mib" 'BEGIN { printf "%.3f", c*1000/m }')"

      echo "$agent,$rep,$chunk,$conns,$sent_mib,$wall_s,$tput,$cpu_s,$cpu_ms_per_mib" | tee -a "$RESULTS"
    done
  done
done

echo
echo "Done. Full results in $RESULTS"
