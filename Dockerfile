FROM debian:latest

ARG TARGETARCH

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /etc/stk /run/stk /root/.ssh \
    && chmod 700 /root/.ssh

COPY --chmod=755 dist/docker/${TARGETARCH}/stk /usr/local/bin/stk
COPY LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.md /usr/share/doc/stk/

LABEL org.opencontainers.image.title="SSH Tunnel Keeper" \
      org.opencontainers.image.description="Reliable SSH proxies and tunnel management" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# The CLI handles SIGINT for graceful shutdown.
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/stk"]
CMD ["serve", "--system", "--config", "/etc/stk"]
