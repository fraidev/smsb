# Stage 1: Build
FROM rust:1.83 as builder

# Set the working directory
WORKDIR /usr/src/app

# Copy the manifest and dependencies
COPY Cargo.toml Cargo.lock ./

# Pre-cache dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src

# Copy the source code
COPY . .

# Build the application
RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim

# Install necessary runtime libraries
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

# Set the working directory
WORKDIR /usr/src/app

# Copy the compiled binary from the builder stage
COPY --from=builder /usr/src/app/target/release/smsb .

# Set the entrypoint
CMD ["./smsb"]
