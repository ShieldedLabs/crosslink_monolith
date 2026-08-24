# Crosslink Monolith - Build Instructions  
**zebrad Ubuntu GUI Binary** 

This guide shows you how to compile the **single GUI-enabled `zebrad` binary** from the ShieldedLabs/crosslink_monolith repository.  


## Quick Start (Ubuntu/Debian)

### 1. Clone the repository and checkout the correct tag

```bash
git clone https://github.com/ShieldedLabs/crosslink_monolith.git
cd crosslink_monolith
git checkout s1_dev          # Change to the tag/version you want
```


### Install Rust (if not already installed)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup update stable
```

### Install system dependencies

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config libssl-dev \
  protobuf-compiler \
  clang libclang-dev \
  libx11-dev libxi-dev libgl1-mesa-dev libasound2-dev \
  libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libxkbcommon-dev libgtk-3-dev
```

### Build the GUI-enabled zebrad binary

```bash
cd zebra-crosslink
cargo build --release --features viz_gui
```
