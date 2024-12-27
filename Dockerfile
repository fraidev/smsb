# Stage 1: Build
FROM rust:1.83 AS builder
WORKDIR /usr/src/app
COPY . .
RUN cargo install --path .

# Stage 2: Runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y \
   ca-certificates \
   openssl \
   && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/cargo/bin/smsb /usr/local/bin/smsb
CMD ["smsb"]
