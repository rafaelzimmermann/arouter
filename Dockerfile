# arouter container — multi-stage build.
# Build from the repo root:  docker build -t arouter:local .
FROM rust:1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
COPY config.example.toml ./
RUN cargo build --release --locked --bin arouter

FROM debian:bookworm-slim
# curl: the quota watcher shells out to user-configured commands
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 1000 arouter
COPY --from=build /build/target/release/arouter /usr/local/bin/arouter
USER arouter
WORKDIR /home/arouter
EXPOSE 3929
ENTRYPOINT ["arouter"]
CMD ["serve", "--config", "/etc/arouter/arouter.toml"]
