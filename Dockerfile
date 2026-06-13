FROM rust:latest AS builder

WORKDIR /app
COPY . .
RUN cargo build --release

FROM ubuntu:24.04

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system herd \
    && useradd --system --gid herd --home-dir /nonexistent --shell /usr/sbin/nologin herd \
    # Create the persistent data directory and hand it to the herd user.
    # The home dir stays /nonexistent — HERD_DATA_DIR is set explicitly below
    # so nothing ever falls back to the home directory.
    && mkdir -p /var/lib/herd \
    && chown herd:herd /var/lib/herd

COPY --from=builder /app/target/release/herd /usr/local/bin/herd

EXPOSE 40114

# All gateway stores (node DB, analytics, audit, sessions, costs, binaries)
# root under HERD_DATA_DIR. Mount a volume here to persist data across restarts.
ENV HERD_DATA_DIR=/var/lib/herd

VOLUME /var/lib/herd

USER herd

ENTRYPOINT ["herd"]
CMD ["--config", "/etc/herd/herd.yaml"]
