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
RUN useradd --system --uid 10001 --home /data llma && mkdir -p /data /etc/llm-accounting && chown llma /data
COPY --from=build /src/target/release/llm-accounting /usr/local/bin/llm-accounting
COPY config.example.toml /etc/llm-accounting/config.toml
USER llma
WORKDIR /data
ENV LLM_ACCOUNTING_LISTEN=0.0.0.0:8787 \
    LLM_ACCOUNTING_DATABASE=sqlite:///data/llm-accounting.db
EXPOSE 8787
VOLUME /data
# Optional seed: mount a file and pass --config /etc/llm-accounting/config.toml.
CMD ["llm-accounting"]
