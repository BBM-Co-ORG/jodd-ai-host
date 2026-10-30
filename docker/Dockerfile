# Jodd AI host: OpenSSH plus a dedicated account for the claude / codex CLIs, and jodd-ai-mcp,
# the program Jodd's keys run. jodd-ai-mcp is built from this repository below (it is ours to
# ship). The CLIs are NOT in this image: they install from their official installers into the
# account's home volume on first start, on your machine.
# Build from the repository root: docker build -f docker/Dockerfile .
# Guide: https://github.com/BBM-Co-ORG/jodd-ai-host/blob/main/docs/INSTALL.md

# ---- jodd-ai-mcp, static (musl): Rust on Alpine targets musl and links it statically.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release --locked -p jodd-ai-mcp \
 && mkdir -p /out && cp target/release/jodd-ai-mcp /out/jodd-ai-mcp \
 && /out/jodd-ai-mcp --version

# ---- the host
FROM debian:bookworm-slim
LABEL org.opencontainers.image.source="https://github.com/BBM-Co-ORG/jodd-ai-host" \
      org.opencontainers.image.description="AI host for Jodd: run your own claude / codex CLI over a restricted SSH key" \
      org.opencontainers.image.licenses="Apache-2.0"
RUN apt-get update \
 && apt-get install -y --no-install-recommends openssh-server openssh-client bash curl ca-certificates procps util-linux coreutils ripgrep netcat-openbsd \
 && rm -rf /var/lib/apt/lists/* /etc/ssh/ssh_host_* \
 && useradd -m -s /bin/bash jodd-ai \
 && printf '%s\n' 'export PATH="$HOME/.local/bin:$PATH"' >> /home/jodd-ai/.profile \
 && mkdir -p /run/sshd /var/lib/jodd-ai-host /usr/local/lib/jodd-ai-host
COPY --from=build /out/jodd-ai-mcp /usr/local/bin/jodd-ai-mcp
COPY lib/ /usr/local/lib/jodd-ai-host/
COPY bin/jodd-ai-host /usr/local/bin/jodd-ai-host
COPY docker/entrypoint.sh /usr/local/sbin/jodd-ai-host-entrypoint
# Root-owned, and only root's to change: every Jodd key's forced command is jodd-ai-mcp.
RUN chown root:root /usr/local/bin/jodd-ai-mcp \
 && chmod 755 /usr/local/bin/jodd-ai-mcp /usr/local/bin/jodd-ai-host /usr/local/sbin/jodd-ai-host-entrypoint
EXPOSE 22
VOLUME ["/home/jodd-ai", "/var/lib/jodd-ai-host"]
ENTRYPOINT ["/usr/local/sbin/jodd-ai-host-entrypoint"]
