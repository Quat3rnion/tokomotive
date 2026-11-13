FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
COPY assets ./assets
COPY config.example.toml ./
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home /data tokomotive && mkdir -p /data /etc/tokomotive && chown tokomotive /data
COPY --from=build /src/target/release/tokomotive /usr/local/bin/tokomotive
COPY config.example.toml /etc/tokomotive/config.toml
USER tokomotive
WORKDIR /data
ENV TOKOMOTIVE_LISTEN=0.0.0.0:8787 \
    TOKOMOTIVE_DATABASE=sqlite:///data/tokomotive.db
EXPOSE 8787
VOLUME /data
# Optional seed: mount a file and pass --config /etc/tokomotive/config.toml.
CMD ["tokomotive"]
