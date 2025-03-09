# Stage 1: Build the application
FROM rust:1.65 as builder

# Set the working directory inside the container
WORKDIR /app

# Copy the Cargo.toml and Cargo.lock files first to leverage Docker cache.
COPY Cargo.toml Cargo.lock ./

# Create an empty src directory and copy in your main source files for dependency resolution.
RUN mkdir src
RUN echo "fn main() {}" > src/main.rs

# Download dependencies (this layer will be cached unless Cargo.toml changes)
RUN cargo fetch

# Now copy the actual source code
COPY . .

# Build the release binary
RUN cargo build --release

# Stage 2: Run the application
FROM debian:buster-slim

# Install any required dependencies (if needed; for example, OpenSSL libraries)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binary from the builder stage
COPY --from=builder /app/target/release/mineshield-proxy /usr/local/bin/mineshield-proxy

# Expose the port your proxy listens on (adjust if needed)
EXPOSE 25565

# Set the command to run the proxy
CMD ["mineshield-proxy"]
