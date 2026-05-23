FROM rust:latest AS builder

WORKDIR /app
COPY . .
RUN cargo build --release

FROM ubuntu:24.04

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system herd \
    && useradd --system --gid herd --home-dir /nonexistent --shell /usr/sbin/nologin herd

COPY --from=builder /app/target/release/herd /usr/local/bin/herd

EXPOSE 40114

USER herd

ENTRYPOINT ["herd"]
CMD ["--config", "/etc/herd/herd.yaml"]
