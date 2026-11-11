# Images for the animated README demo; see scripts/demo/record.sh.
# Base images are pinned by digest: update them deliberately.

FROM docker:29-cli@sha256:2357639ad1ea7d0194134bf3292622fd4e562483966ed674e5d7c945b5f3b28d AS docker-cli

# A demo machine: the static musl binary is mounted at run time.
FROM debian:stable-slim@sha256:fbfb4059d2127120aa05b6e6064523d2102552f0cf0026ddf0701c12f1f8f3e0 AS host
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --shell /bin/bash demo
COPY demo.bashrc /etc/demo.bashrc
USER demo
WORKDIR /home/demo
RUN mkdir site && echo "hello from the nas" > site/hello.txt

# The recorder: VHS (with ttyd and ffmpeg), tmux to show both machines side by
# side, and the Docker CLI to open a shell in each of them.
FROM ghcr.io/charmbracelet/vhs:v0.12.1@sha256:ea49a6a1c529be83153e88321892b5585964418f0b9055e8c1e0d732194234a3 AS recorder
RUN apt-get update \
 && apt-get install -y --no-install-recommends tmux \
 && rm -rf /var/lib/apt/lists/*
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
