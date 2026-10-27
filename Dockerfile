FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY plugins plugins
COPY examples examples
RUN cargo build --release --locked -p simpletickets -p simpletickets-mcp -p simpletickets-sync -p simpletickets-connector-archive -p simpletickets-connector-jira

FROM debian:bookworm-slim
RUN useradd --create-home --uid 1000 tickets \
    && mkdir /data \
    && chown tickets /data
COPY --from=build /src/target/release/simpletickets /src/target/release/simpletickets-mcp /src/target/release/simpletickets-sync /src/target/release/simpletickets-connector-archive /src/target/release/simpletickets-connector-jira /usr/local/bin/
# The UI is not included: it listens on 127.0.0.1 only, so it would be unreachable from outside the container.
COPY plugins/mcp/plugin.yml /usr/local/share/simpletickets/plugins/mcp/plugin.yml
COPY plugins/sync/plugin.yml /usr/local/share/simpletickets/plugins/sync/plugin.yml
COPY plugins/connector-archive/plugin.yml /usr/local/share/simpletickets/plugins/connector-archive/plugin.yml
COPY plugins/connector-jira/plugin.yml /usr/local/share/simpletickets/plugins/connector-jira/plugin.yml
ENV SIMPLETICKETS_PLUGINS_DIR=/usr/local/share/simpletickets/plugins
USER tickets
WORKDIR /data
VOLUME /data
EXPOSE 7878
# Bind mounts often deliver no file events, so the container polls; it listens on all
# interfaces inside the container, and the port is published to the host's loopback only.
# The API requires a token on every route, health included, so the check only confirms
# that the daemon accepts TCP connections (bash is part of the base image; curl is not).
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/7878"]
ENTRYPOINT ["simpletickets", "--dir", "/data"]
CMD ["daemon", "--bind-all", "--poll", "--port", "7878"]
