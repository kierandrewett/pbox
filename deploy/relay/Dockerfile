FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY proto ./proto
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*
RUN cargo build --locked --release -p pbox-relay

FROM debian:13-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/pbox-relay /usr/local/bin/pbox-relay
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["pbox-relay"]
CMD ["--key-file", "/run/secrets/relay-key"]
