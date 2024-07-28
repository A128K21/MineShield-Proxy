FROM rust:latest

# Install necessary tools
RUN apt-get update && apt-get install -y \
    gcc-aarch64-linux-gnu \
    g++-aarch64-linux-gnu \
    libc6-dev-arm64-cross

# Add the target for aarch64
RUN rustup target add aarch64-unknown-linux-gnu

# Set the working directory
WORKDIR /usr/src/myapp

# Copy the source code into the Docker image
COPY . .

# Set environment variable and build the project
CMD ["sh", "-c", "RUST_LOG=info cargo build --release --target=aarch64-unknown-linux-gnu"]
