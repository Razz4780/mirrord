# Benchmark image carrying both agents and the echo server; which one runs is
# decided by the pod's command. Ubuntu matches the glibc of the build host - this
# image is only ever built from locally-built binaries and loaded into kind.
FROM ubuntu:24.04
COPY mirrord-agent /mirrord-agent
COPY mirrord-agent-ractor /mirrord-agent-ractor
COPY echo_server /echo_server
