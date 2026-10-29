FROM rust:1.98-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:trixie-slim
RUN useradd --system --uid 10001 --create-home app
WORKDIR /app
COPY --from=build /src/target/release/cnb-forge /usr/local/bin/cnb-forge
COPY config.example.yaml ./config.example.yaml
USER app
EXPOSE 8080
ENTRYPOINT ["cnb-forge", "--host", "0.0.0.0"]
