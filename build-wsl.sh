#!/bin/bash

# Source the Rust environment explicitly
source $HOME/.cargo/env

# Define variables
PROJECT_DIR="/mnt/c/Users/akosv/RustroverProjects/mineshieldv2-proxy"
OUTPUT_DIR="$PROJECT_DIR/out"
BINARY_NAME="mineshieldv2-proxy"  # Replace with your actual binary name
TARGET_DIR="$PROJECT_DIR/target/release"

# Navigate to the project directory
cd "$PROJECT_DIR" || { echo "Project directory not found."; exit 1; }

# Ensure the output directory exists
mkdir -p "$OUTPUT_DIR"

# Build the Rust project using all available cores
echo "Building the project..."
cargo build --release -j 16

# Check if the build was successful
if [ $? -eq 0 ]; then
    # Move the built binary to the output directory
    if [ -f "$TARGET_DIR/$BINARY_NAME" ]; then
        mv "$TARGET_DIR/$BINARY_NAME" "$OUTPUT_DIR"
        echo "Build process complete. The binary is located in the $OUTPUT_DIR directory."
    else
        echo "Build finished, but the binary was not found."
    fi
else
    echo "Build failed."
fi
