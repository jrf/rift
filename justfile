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
    cp target/release/rif ~/.local/bin/
    codesign -s - ~/.local/bin/rif

# Uninstall from ~/.local/bin
uninstall:
    rm -f ~/.local/bin/rif

# Remove build artifacts
clean:
    cargo clean
