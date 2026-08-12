# Justfile

# list all tasks
default:
  @just --list

# Run all style checks and formatting (precommit validation)
check-everything:
    @echo "🔧 RUNNING ALL STYLE CHECKS..."
    @echo "  → Formatting Rust code..."
    cargo fmt --all
    @echo "  → Running clippy linting..."
    cargo clippy --all-targets -- -D warnings
    @echo ""
    @echo "✅ All style checks passed!"

# Default release command
release-binary:
    @echo "Building release version..."
    cargo build --release -p kaji-cli --bin kaji

# Build Windows executable on a Windows host
[unix]
release-windows:
    @echo "just release-windows requires a Windows host because Kaji Windows releases build the MSVC target. Use .github/workflows/bundle-windows.yml for CI builds."
    @exit 1

[windows]
release-windows:
    @powershell.exe -NoProfile -ExecutionPolicy Bypass -Command 'rustup target add x86_64-pc-windows-msvc; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; cargo build --release --target x86_64-pc-windows-msvc -p kaji-cli --bin kaji; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; Write-Host "Windows executable created at ./target/x86_64-pc-windows-msvc/release/kaji.exe"'

# Build for Intel Mac
release-intel:
    @echo "Building release version for Intel Mac..."
    cargo build --release --target x86_64-apple-darwin

# Run Docusaurus server for documentation
run-docs:
    @echo "Running docs server..."
    cd documentation && yarn && yarn start

# Run server
run-server:
    @echo "Running external ACP backend..."
    KAJI_SERVER__SECRET_KEY="${KAJI_SERVER__SECRET_KEY:-test}" cargo run -p kaji-cli --bin kaji -- serve --platform desktop --enable-scheduler --host 127.0.0.1 --port 3000

# Check if generated ACP schema and TypeScript types are up-to-date
check-acp-schema: generate-acp-types
    #!/usr/bin/env bash
    set -e
    echo "🔍 Checking ACP schema and generated types are up-to-date..."
    if ! git diff --exit-code crates/kaji/acp-schema.json crates/kaji/acp-meta.json ui/sdk/src/generated/; then
      echo ""
      echo "❌ ACP generated files are out of date!"
      echo ""
      echo "Run 'just generate-acp-types' locally, then commit the changes."
      exit 1
    fi
    echo "✅ ACP schema and generated types are up-to-date"

# Generate ACP JSON schema from Rust types
generate-acp-schema:
    @echo "Generating ACP schema..."
    cd crates/kaji && cargo run --features code-mode,local-inference,aws-providers,telemetry,otel,rustls-tls,system-keyring --bin generate-acp-schema
    @echo "ACP schema generated: crates/kaji/acp-schema.json, crates/kaji/acp-meta.json"

# Generate ACP TypeScript types from JSON schema (requires generate-acp-schema first)
generate-acp-types: generate-acp-schema
    @echo "Generating ACP TypeScript types..."
    cd ui/sdk && npx tsx generate-schema.ts
    @echo "ACP TypeScript types generated in ui/sdk/src/generated/"

# Build SDK TypeScript package (schema + types + compile)
build-sdk: generate-acp-types
    @echo "Compiling ACP TypeScript..."
    cd ui/sdk && pnpm run build:ts
    @echo "ACP package built."

# Generate manpages for the CLI
generate-manpages:
    @echo "Generating manpages..."
    cargo run -p kaji-cli --bin generate_manpages
    @echo "Manpages generated at target/man/"

# Install all dependencies (run once after fresh clone)
install-deps:
    cd documentation && yarn

ensure-release-branch:
    #!/usr/bin/env bash
    branch=$(git rev-parse --abbrev-ref HEAD); \
    if [[ ! "$branch" == release/* ]]; then \
        echo "Error: You are not on a release branch (current: $branch)"; \
        exit 1; \
    fi

    # check that main is up to date with upstream main
    git fetch
    # @{u} refers to upstream branch of current branch
    if [ "$(git rev-parse HEAD)" != "$(git rev-parse @{u})" ]; then \
        echo "Error: Your branch is not up to date with the upstream branch"; \
        echo "  ensure your branch is up to date (git pull)"; \
        exit 1; \
    fi

# validate the version is semver, and not the current version
validate version:
    #!/usr/bin/env bash
    if [[ ! "{{ version }}" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-.*)?$ ]]; then
      echo "[error]: invalid version '{{ version }}'."
      echo "  expected: semver format major.minor.patch or major.minor.patch-<suffix>"
      exit 1
    fi

    current_version=$(just get-tag-version)
    if [[ "{{ version }}" == "$current_version" ]]; then
      echo "[error]: current_version '$current_version' is the same as target version '{{ version }}'"
      echo "  expected: new version in semver format"
      exit 1
    fi

get-next-minor-version:
    @python -c "import sys; v=sys.argv[1].split('.'); print(f'{v[0]}.{int(v[1])+1}.0')" $(just get-tag-version)

get-next-patch-version:
    @python -c "import sys; v=sys.argv[1].split('.'); print(f'{v[0]}.{v[1]}.{int(v[2])+1}')" $(just get-tag-version)

# derive the prior release tag from a version
# patch bump (e.g. 1.25.1): prior is v1.25.0 (deterministic)
# minor bump (e.g. 1.26.0): prior is highest v1.25.* GitHub release
get-prior-version version:
    #!/usr/bin/env bash
    IFS='.' read -r major minor patch <<< "{{ version }}"
    if [[ "$patch" -gt 0 ]]; then
      echo "v${major}.${minor}.$((patch - 1))"
    elif [[ "$minor" -gt 0 ]]; then
      prev_minor=$((minor - 1))
      prefix="v${major}.${prev_minor}."
      best=$(gh release list --limit 100 --exclude-drafts --exclude-pre-releases \
        --json tagName --jq "[.[] | select(.tagName | startswith(\"${prefix}\"))][0].tagName")
      if [[ -n "$best" && "$best" != "null" ]]; then
        echo "$best"
      fi
    fi

# update version numbers in all manifests
bump-version version:
    @just validate {{ version }} || exit 1
    @uvx --from=toml-cli toml set --toml-path=Cargo.toml "workspace.package.version" {{ version }}
    # update Cargo.lock after bumping versions in Cargo.toml
    @cargo update --workspace

# rebuild canonical model registry and mapping report from models.dev
build-canonical-models:
    @cargo run --bin build_canonical_models

# bump version, rebuild canonical models, and commit
prepare-release version:
    @just bump-version {{ version }}
    @just build-canonical-models
    @git add \
        Cargo.toml \
        Cargo.lock \
        ui/pnpm-lock.yaml \
        crates/kaji-provider-types/src/canonical/data/canonical_models.json \
        crates/kaji-provider-types/src/canonical/data/provider_metadata.json
    @git commit --message "chore(release): release version {{ version }}"

# extract version from Cargo.toml
get-tag-version:
    @uvx --from=toml-cli toml get --toml-path=Cargo.toml "workspace.package.version"

# create the git tag from Cargo.toml, checking we're on a release branch
tag: ensure-release-branch
    git tag v$(just get-tag-version)

# create tag and push to origin (use this when release branch is merged to main)
tag-push: tag
    # this will kick of ci for release
    git push origin tag v$(just get-tag-version)

# generate release notes from git commits
release-notes old:
    #!/usr/bin/env bash
    git log --pretty=format:"- %s" {{ old }}..v$(just get-tag-version)

### s = file separator based on OS
s := if os() == "windows" { "\\" } else { "/" }
linux_vulkan_features := if os() == "linux" { "--features vulkan" } else { "" }

### testing/debugging
os:
  echo "{{os()}}"
  echo "{{s}}"

# Make just work on Window
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

### Build the core code
### profile = --release or "" for debug
### allparam = OR/AND/ANY/NONE --workspace --all-features --all-targets
win-bld profile allparam:
  cargo build {{profile}} {{allparam}}

### Build just debug
win-bld-dbg:
  just win-bld " " " "

### Build debug and test, examples,...
win-bld-dbg-all:
  just win-bld " " "--workspace --all-targets --all-features"

### Build just release
win-bld-rls:
  just win-bld "--release" " "

### Build release and test, examples, ...
win-bld-rls-all:
  just win-bld "--release" "--workspace --all-targets --all-features"

build-test-tools:
  cargo build -p kaji-test

record-mcp-tests: build-test-tools
  KAJI_RECORD_MCP=1 cargo test --package kaji --test mcp_integration_test
  git add crates/kaji/tests/mcp_replays/

# Build, install (unlink first — macOS SIGKILLs overwritten live binaries) and
# codesign ~/.local/bin/kaji with a stable identity so the Keychain ACL
# survives rebuilds (ad-hoc signing changes the cdhash on every build and
# re-triggers the password prompt).
install codesign_id="Apple Development: Jean-Paul Lamy (MLLVU787AX)":
    cargo build --release -p kaji-cli --bin kaji
    rm -f ~/.local/bin/kaji
    cp target/release/kaji ~/.local/bin/kaji
    codesign --force -s "{{codesign_id}}" ~/.local/bin/kaji
    @echo "installé + signé : $(~/.local/bin/kaji --version)"
