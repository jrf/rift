default: install

# Build in debug mode
build:
    cargo build

# Build in release mode
release:
    cargo build --release

# Run the app
run:
    cargo run

# Install to ~/.local/bin
install: release
    cp target/release/rift ~/.local/bin/
    codesign -s - ~/.local/bin/rift

# Uninstall from ~/.local/bin
uninstall:
    rm -f ~/.local/bin/rift

# Remove build artifacts
clean:
    cargo clean
