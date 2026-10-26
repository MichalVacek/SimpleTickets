FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY plugins plugins
RUN cargo build --release --locked -p simpletickets -p simpletickets-mcp

FROM debian:bookworm-slim
RUN useradd --create-home --uid 1000 tickets \
    && mkdir /data \
    && chown tickets /data
COPY --from=build /src/target/release/simpletickets /src/target/release/simpletickets-mcp /usr/local/bin/
USER tickets
WORKDIR /data
VOLUME /data
EXPOSE 7878
# Bind mounts often deliver no file events, so the container polls; it listens on all
# interfaces inside the container, and the port is published to the host's loopback only.
ENTRYPOINT ["simpletickets", "--dir", "/data"]
CMD ["daemon", "--bind-all", "--poll", "--port", "7878"]
