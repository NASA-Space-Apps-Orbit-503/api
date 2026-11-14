FROM rust:1-bookworm AS build
WORKDIR /app

# Primero solo las dependencias, para que Docker las cachee:
# compilar DuckDB tarda varios minutos y no queremos repetirlo en cada cambio.
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
 && cargo build --release \
 && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/api-nasa /usr/local/bin/api-nasa
ENV PORT=8080
EXPOSE 8080
CMD ["api-nasa"]
