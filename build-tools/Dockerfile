# Use the Rust image and specify the platform as linux/arm64
FROM --platform=linux/arm64 rust:latest

# Install necessary tools including sccache
RUN apt-get update && apt-get install -y \
    libssl-dev \
    pkg-config \
    curl \
    && cargo install sccache

# Set up sccache as the Rust compiler wrapper
ENV RUSTC_WRAPPER=sccache

# Set the working directory
WORKDIR /usr/src/myapp

# Copy only the Cargo.toml and Cargo.lock files first to cache dependencies
COPY ../Cargo.toml Cargo.lock ./

# Fetch dependencies to cache them
RUN cargo fetch

# Copy the source code into the Docker image
COPY .. .

# Enable incremental compilation and set the number of jobs to 16 for parallel builds
ENV CARGO_INCREMENTAL=1

# Build the project using all 16 cores
CMD ["sh", "-c", "RUST_LOG=info cargo build --release -j 16"]
