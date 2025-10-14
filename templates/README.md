# sy Ignore Templates

This directory contains example ignore templates for common project types.

## Usage

### Option 1: Copy to your config directory

```bash
# Create templates directory
mkdir -p ~/.config/sy/templates

# Copy templates you want
cp templates/*.syignore ~/.config/sy/templates/

# Use with --ignore-template flag
sy /source /dest --ignore-template rust
sy /source /dest --ignore-template node --ignore-template python
```

### Option 2: Use .syignore files in your project

Create a `.syignore` file in your project root (similar to `.gitignore`):

```bash
# Create .syignore
cat > /path/to/project/.syignore <<'EOF'
# Project-specific ignore patterns
target/
node_modules/
__pycache__/
EOF

# sy will automatically load .syignore from the source directory
sy /path/to/project /backup
```

## Available Templates

- **rust.syignore** - For Rust projects (target/, Cargo.lock, etc.)
- **node.syignore** - For Node.js projects (node_modules/, dist/, logs/, etc.)
- **python.syignore** - For Python projects (\_\_pycache\_\_/, .venv/, dist/, etc.)

## Pattern Syntax

Templates use rsync-style filter syntax:

```
# Comments start with #
*.log          # Exclude all .log files
target/        # Exclude target directory and all contents
+ *.txt        # Include .txt files (use + for explicit includes)
- *.tmp        # Exclude .tmp files (- prefix optional, exclude is default)
```

## Priority Order

When multiple filter sources are used, they are applied in this order:

1. `--filter` CLI flags (highest priority)
2. `--include` and `--exclude` CLI flags
3. `.syignore` file in source directory
4. `--ignore-template` templates
5. `.gitignore` patterns (lowest priority)

First matching rule wins!

## Creating Custom Templates

Create your own templates in `~/.config/sy/templates/`:

```bash
# Create a custom template
cat > ~/.config/sy/templates/myproject.syignore <<'EOF'
# Custom ignore patterns for my project
build/
cache/
*.tmp
EOF

# Use it
sy /source /dest --ignore-template myproject
```

## Examples

```bash
# Rust project backup
sy ~/rust-project ~/backups/rust --ignore-template rust

# Node.js project with custom .syignore
cd ~/node-app
echo "node_modules/" > .syignore
echo "dist/" >> .syignore
sy . ~/backups/node-app

# Multiple templates
sy ~/monorepo ~/backups --ignore-template rust --ignore-template node

# Combine with CLI patterns
sy ~/project ~/backup --ignore-template rust --exclude "*.bak"
```
