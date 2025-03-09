#!/bin/bash

# Define variables
IMAGE_NAME="my-rust-cross-compiler"
CONTAINER_NAME="my-rust-builder"
BINARY_NAME="mineshieldv2-proxy"  # Replace with your actual binary name
OUTPUT_DIR="output/"

# Step 1: Create Dockerfile
cat << 'EOF' > Dockerfile
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
EOF

# Step 2: Build the Docker image
echo "Building Docker image..."
docker build -t $IMAGE_NAME .

# Step 3: Run the Docker container
echo "Running Docker container..."
docker run --name $CONTAINER_NAME -v "$(pwd)":/usr/src/myapp -w /usr/src/myapp $IMAGE_NAME sh -c "cargo build --release --target=aarch64-unknown-linux-gnu && tail -f /dev/null"

# Step 4: Copy the built binary out of the container
echo "Copying the built binary out of the container..."
mkdir -p $OUTPUT_DIR
docker cp $CONTAINER_NAME:/usr/src/myapp/target/aarch64-unknown-linux-gnu/release/$BINARY_NAME $OUTPUT_DIR/

# Step 5: Stop and remove the Docker container
echo "Stopping and removing the Docker container..."
docker stop $CONTAINER_NAME
docker rm $CONTAINER_NAME

echo "Build process complete. The binary is located in the $OUTPUT_DIR directory."
