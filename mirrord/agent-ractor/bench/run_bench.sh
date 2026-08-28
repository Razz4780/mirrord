#!/usr/bin/env bash
# CPU-cost benchmark: original mirrord-agent vs mirrord-agent-ractor.
#
# Prerequisites (see bench/README.md):
#   * a kind cluster with bench/k8s.yaml applied (image mirrord-bench:latest loaded),
#   * bench_client built on the host (cargo build -p mirrord-agent-ractor --release --bins).
#
# For every (agent, chunk size, connection count) combination this script runs
# the spam client REPS times and reports, per run, the agent's CPU time consumed
# per MiB pushed through it. CPU time is read from /proc/1/stat (utime+stime) of
# the agent process itself, so pause containers, exec probes etc. are excluded;
# cgroup cpu.stat is captured too as a cross-check.

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

# agent_cpu_ticks <pod>: cumulative utime+stime of the agent process, in USER_HZ
# (100/s) ticks. PID 1 in the container is the agent itself.
agent_cpu_ticks() {
  kubectl exec "$1" -- cat /proc/1/stat | awk '{ print $14 + $15 }'
}

# agent_cgroup_usec <pod>: cumulative container CPU from cgroup v2, microseconds.
agent_cgroup_usec() {
  kubectl exec "$1" -- sh -c 'grep ^usage_usec /sys/fs/cgroup/cpu.stat' | awk '{ print $2 }'
}

echo "node=$NODE_IP echo=$ECHO_IP client=$BENCH_CLIENT reps=$REPS"
echo "agent,run,chunk_kib,conns,sent_mib,wall_s,throughput_mib_s,proc_cpu_s,cgroup_cpu_s,proc_cpu_ms_per_mib" | tee "$RESULTS"

# Matrix: 64KiB chunks measure bulk relaying, 4KiB chunks stress the
# per-message machinery, which is where an actor framework's overhead lives.
for agent in original ractor; do
  pod="${AGENT_PODS[$agent]}"
  port="${AGENT_PORTS[$agent]}"

  # Warmup: populates page cache, JITs nothing but settles allocators and TCP.
  "$BENCH_CLIENT" --agent "$NODE_IP:$port" --target "$ECHO_IP:7777" \
    --total-mib 256 --chunk-kib 64 --conns 4 > /dev/null

  for spec in "64 4" "4 4" "64 1"; do
    read -r chunk conns <<< "$spec"
    total_mib=$(( chunk >= 64 ? 2048 : 512 ))
    for rep in $(seq 1 "$REPS"); do
      cpu0="$(agent_cpu_ticks "$pod")"
      cg0="$(agent_cgroup_usec "$pod")"
      out="$("$BENCH_CLIENT" --agent "$NODE_IP:$port" --target "$ECHO_IP:7777" \
        --total-mib "$total_mib" --chunk-kib "$chunk" --conns "$conns" | grep ^RESULT)"
      cpu1="$(agent_cpu_ticks "$pod")"
      cg1="$(agent_cgroup_usec "$pod")"

      sent_mib="$(sed -n 's/.*sent_mib=\([0-9.]*\).*/\1/p' <<< "$out")"
      wall_s="$(sed -n 's/.*wall_s=\([0-9.]*\).*/\1/p' <<< "$out")"
      tput="$(sed -n 's/.*throughput_mib_s=\([0-9.]*\).*/\1/p' <<< "$out")"
      proc_cpu_s="$(awk -v a="$cpu0" -v b="$cpu1" 'BEGIN { printf "%.2f", (b-a)/100 }')"
      cgroup_cpu_s="$(awk -v a="$cg0" -v b="$cg1" 'BEGIN { printf "%.2f", (b-a)/1000000 }')"
      cpu_ms_per_mib="$(awk -v c="$proc_cpu_s" -v m="$sent_mib" 'BEGIN { printf "%.3f", c*1000/m }')"

      echo "$agent,$rep,$chunk,$conns,$sent_mib,$wall_s,$tput,$proc_cpu_s,$cgroup_cpu_s,$cpu_ms_per_mib" | tee -a "$RESULTS"
    done
  done
done

echo
echo "Done. Full results in $RESULTS"
