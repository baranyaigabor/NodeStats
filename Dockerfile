FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && mkdir /state && chown 65532:65532 /state
COPY --from=build /src/target/release/nodestats /usr/local/bin/nodestats
ENV NODESTATS_STATE_DIR=/state
USER 65532:65532
ENTRYPOINT ["nodestats"]
CMD ["run"]
