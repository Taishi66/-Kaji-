---
sidebar_position: 7
title: CLI Commands
sidebar_label: CLI Commands
toc_max_heading_level: 4
---

import Tabs from '@theme/Tabs';
import TabItem from '@theme/TabItem';

kaji provides a command-line interface (CLI) with commands for managing sessions, configurations and extensions. This guide covers the main CLI commands and interactive session features.

## Flag Naming Conventions

kaji CLI follows consistent patterns for flag naming to make commands intuitive and predictable:

- **`--session-id`**: Used for session identifiers (e.g., `20251108_1`)
- **`--schedule-id`**: Used for schedule job identifiers (e.g., `daily-report`)
- **`-n, --name`**: Used for human-readable names
- **`--path`**: Used for file paths (legacy support)
- **`-o, --output`**: Used for output file paths
- **`-r, --resume` or `-r, --regex`**: Context-dependent (resume for sessions, regex for filters)
- **`-v, --verbose`**: Used for verbose output
- **`-l, --limit`**: Used for limiting result counts
- **`-f, --format`**: Used for specifying output formats
- **`-w, --working_dir`**: Used for working directory filters

### Core Commands

#### help
Display the help menu.

**Usage:**
```bash
kaji --help
```

---

#### configure
Configure kaji settings - providers, extensions, etc.

**Usage:**
```bash
kaji configure
```

:::tip Type to Filter
When selecting from menus in `kaji configure`, start typing to filter options in real-time. This works for lists of providers, extensions, and tools.
:::

---

#### info [options]
Shows kaji information, including the version, configuration file location, session storage, and logs.

**Options:**
- **`-v, --verbose`**: Show detailed configuration settings, including environment variables and enabled extensions

**Usage:**
```bash
kaji info
```

---

#### version
Check the current kaji version you have installed.

**Usage:**
```bash
kaji --version
```

---

#### update [options]
Update the kaji CLI to a newer version.

**Options:**
- **`--canary, -c`**: Update to the canary (development) version instead of the stable version
- **`--reconfigure, -r`**: Forces kaji to reset configuration settings during the update process

**Usage:**
```bash
# Update to latest stable version
kaji update

# Update to latest canary version
kaji update --canary

# Update and reconfigure settings
kaji update --reconfigure
```

---

#### completion
Generate shell-specific scripts to enable tab completion of kaji commands, subcommands, and options. The script is printed to stdout, so you need to redirect it to the appropriate location for your shell and then reload or source your shell configuration.

Once installed, you can:
- Press Tab to see available commands and subcommands
- Complete command names and flags automatically
- Discover options without checking `--help`

**Arguments:**
- **`<SHELL>`**: The shell to generate completions for. Supported shells: `bash`, `elvish`, `fish`, `nu`, `powershell`, `zsh`

**Usage:**
```bash
# Generate completion script for your shell (outputs to stdout)
kaji completion bash
kaji completion zsh
kaji completion fish
kaji completion nu
```

**Installation by Shell:**

<Tabs groupId="shells">
<TabItem value="zsh" label="Zsh" default>

Add this line to your `~/.zshrc`:

```bash
eval "$(kaji completion zsh)"
```

Then reload your shell:
```bash
source ~/.zshrc
```

</TabItem>
<TabItem value="bash" label="Bash">

Add this line to your `~/.bashrc` or `~/.bash_profile`:

```bash
eval "$(kaji completion bash)"
```

Then reload your shell:
```bash
source ~/.bashrc
```

</TabItem>
<TabItem value="fish" label="Fish">

```bash
kaji completion fish > ~/.config/fish/completions/kaji.fish
```

Then restart your terminal or run `exec fish`.

</TabItem>
<TabItem value="nu" label="Nushell">

```nu
let autoload_dir = ($nu.user-autoload-dirs | first)
mkdir $autoload_dir
kaji completion nu | save --force ($autoload_dir | path join "kaji.nu")
```

Then restart Nushell or run:
```nu
source (($nu.user-autoload-dirs | first) | path join "kaji.nu")
```

</TabItem>
<TabItem value="powershell" label="PowerShell">

Add this line to your PowerShell profile:

```powershell
kaji completion powershell | Out-String | Invoke-Expression
```

Then reload your profile:
```powershell
. $PROFILE
```

</TabItem>
</Tabs>

:::tip Testing
After installing and reloading your shell, test completion by typing `kaji ` and pressing Tab to see available commands, or `kaji session --` and Tab to see available options.
:::

---

### Session Management

:::info Session Storage Migration
Starting with version 1.10.0, kaji uses a SQLite database (`sessions.db`) instead of individual `.jsonl` files.
Your existing sessions are automatically imported to the database. Legacy `.jsonl` files remain on disk but are no longer managed by kaji.
:::

#### session [options]
Start or resume interactive chat sessions.

**Basic Options:**
- **`--session-id <session_id>`**: Specify a session by its ID (e.g., '20251108_1')
- **`-n, --name <name>`**: Give the session a name
- **`--path <path>`**: Legacy parameter for specifying session by file path
- **`-r, --resume`**: Resume a previous session
- **`--edit`**: Open the session's conversation in your editor (`$VISUAL` / `$EDITOR` / `vi`) as YAML. Edit, trim, or rewrite messages, then save and close to continue the session with the edited conversation. Must be used with `--resume`. Can be combined with `--fork` to create a new session from the edited result.
- **`--fork`**: Create a new duplicate session with copied history. Must be used with `--resume`. Provide `--name` or `--session-id` to fork a specific session. Otherwise, forks the most recent session.
- **`--history`**: Show previous messages when resuming a session
- **`--container <container_id>`**: Run extensions inside a [Docker container](/docs/tutorials/kaji-in-docker#running-extensions-in-docker-containers).
- **`--debug`**: Enable debug mode to output complete tool responses, detailed parameter values, and full file paths
- **`--max-tool-repetitions <NUMBER>`**: Set the maximum number of times the same tool can be called consecutively with identical parameters. Helps prevent infinite loops.
- **`--max-turns <NUMBER>`**: Set the maximum number of turns allowed without user input (default: 1000)

**Extension Options:**
- **`--with-extension <command>`**: Add stdio extensions
- **`--with-streamable-http-extension <url>`**: Add remote extensions over Streamable HTTP
- **`--with-builtin <id>`**: Enable built-in extensions (e.g., 'developer', 'computercontroller')

**Usage:**
```bash
# Start a basic session
kaji session -n my-project

# Resume a previous session
kaji session --resume -n my-project
kaji session --resume --session-id 20251108_2
kaji session --resume --path ./session.json    # exported session
kaji session --resume --path ./session.jsonl   # legacy session storage

# Fork a specific session by name
kaji session --resume --fork --name my-project

# Fork the most recent session and show message history
kaji session --resume --fork --history

# Edit a session's conversation in your editor
kaji session --resume --session-id 20251108_2 --edit

# Edit and fork — create a new session from the edited conversation
kaji session --resume --session-id 20251108_2 --fork --edit --history

# Start with extensions
kaji session --with-extension "npx -y @modelcontextprotocol/server-memory"
kaji session --with-builtin developer
kaji session --with-streamable-http-extension "http://localhost:8080/mcp"

# Advanced: Mix multiple extension types
kaji session \
  --with-extension "echo hello" \
  --with-streamable-http-extension "http://localhost:8080/mcp" \
  --with-builtin "developer"

# Control session behavior
kaji session -n my-session --debug --max-turns 25
```

---

#### session list [options]
List all saved sessions.

**Options:**
- **`-f, --format <format>`**: Specify output format (`text` or `json`). Default is `text`
- **`--ascending`**: Sort sessions by date in ascending order (oldest first)
- **`-w, --working_dir <path>`**: Filter sessions by working directory
- **`-l, --limit <number>`**: Limit the number of results

**Usage:**
```bash
# List all sessions in text format (default)
kaji session list

# List sessions in JSON format
kaji session list --format json

# Sort sessions by date in ascending order
kaji session list --ascending

# Filter sessions by working directory
kaji session list -w ~/projects/myapp

# List only the 10 most recent sessions
kaji session list --limit 10
```

---

#### session remove [options]
Remove one or more saved sessions.

**Options:**
- **`--session-id <session_id>`**: Remove a specific session by its session ID
- **`-n, --name <name>`**: Remove a specific session by its name
- **`-r, --regex <pattern>`**: Remove sessions matching a regex pattern
- **`--path <path>`**: Remove a specific session by its file path (legacy)

**Usage:**
```bash
# Interactive removal (prompts you to choose sessions)
kaji session remove

# Remove a specific session by ID
kaji session remove --session-id 20251108_3

# Remove a specific session by name
kaji session remove -n my-project

# Remove all sessions starting with "project-"
kaji session remove -r "project-.*"

# Remove all sessions containing "migration"
kaji session remove -r ".*migration.*"
```

:::caution
Session removal is permanent and cannot be undone. kaji will show which sessions will be removed and ask for confirmation before deleting.
:::

---

#### session export [options]
Export sessions in different formats for backup, sharing, migration, or documentation purposes.

**Options:**
- **`--session-id <session_id>`**: Export a specific session by ID
- **`-n, --name <name>`**: Export a specific session by name
- **`--path <path>`**: Export a specific session by file path (legacy)
- **`-o, --output <file>`**: Save exported content to a file (default: stdout)
- **`--format <format>`**: Output format: `markdown`, `json`, `yaml`. Default is `markdown`

**Export Formats:**
- **`json`**: Complete session backup preserving all data including conversation history, metadata, and settings
- **`yaml`**: Complete session backup in YAML format
- **`markdown`**: Default format that creates a formatted, readable version of the conversation for documentation and sharing

**Usage:**
```bash
# Interactive export
kaji session export

# Export specific session as JSON for backup
kaji session export -n my-session --format json -o session-backup.json

# Export specific session as readable markdown
kaji session export -n my-session -o session.md

# Export to stdout in different formats
kaji session export --session-id 20251108_4 --format json
kaji session export -n my-session --format yaml

# Export session by path (legacy)
kaji session export --path ./my-session.jsonl -o exported.md
```

---

#### session diagnostics [options]
Generate a comprehensive diagnostics JSON report for troubleshooting issues with a specific session.

**Options:**
- **`--session-id <session_id>`**: Generate diagnostics for a specific session by ID
- **`-n, --name <name>`**: Generate diagnostics for a specific session by name
- **`--path <path>`**: Generate diagnostics for a specific session by file path (legacy)
- **`-o, --output <file>`**: Save diagnostics report to a specific file path (default: `diagnostics_{session_id}.json`)

**What's included:**
- **System Information**: App version, operating system, architecture, and timestamp
- **Session Data**: Complete conversation messages and history for the specified session
- **Configuration Files**: Your [configuration files](/docs/guides/config-files) (if they exist)
- **Log Files**: Recent application logs for debugging

**Usage:**
```bash
# Generate diagnostics for a specific session by ID
kaji session diagnostics --session-id 20251108_5

# Generate diagnostics for a session by name
kaji session diagnostics -n my-project-session

# Save diagnostics to a custom location
kaji session diagnostics --session-id 20251108_5 -o /path/to/my-diagnostics.json

# Interactive selection (prompts you to choose a session)
kaji session diagnostics
```

:::warning Privacy Notice
Diagnostics reports contain your session messages and system information. If your session includes sensitive data (API keys, personal information, proprietary code), review the contents before sharing publicly.
:::

:::tip
Generate diagnostics before reporting bugs to provide technical details that help with faster resolution. The JSON file can be attached to GitHub issues or shared with support.
:::

---

### Task Execution

#### run [options]
Execute commands from an instruction file or stdin. Check out the [full guide](/docs/guides/running-tasks) for more info.

**Input Options:**
- **`-i, --instructions <FILE>`**: Path to instruction file containing commands. Use `-` for stdin
- **`-t, --text <TEXT>`**: Input text to provide to kaji directly
- **`--system <TEXT>`**: Provide additional system instructions to customize the agent's behavior
- **`--recipe <RECIPE_FILE_NAME> <OPTIONS>`**: Load a custom recipe in current session
- **`--params <KEY=VALUE>`**: Key-value parameters to pass to the recipe file. Can be specified multiple times
- **`--sub-recipe <RECIPE>`**: Specify sub-recipes to include alongside the main recipe. Can be specified multiple times

**Session Options:**
- **`-s, --interactive`**: Continue in interactive mode after processing initial input
- **`-n, --name <name>`**: Name for this run session (e.g. `daily-tasks`)
- **`-r, --resume`**: Resume from a previous run
- **`--path <PATH>`**: Path for this run session (e.g. `./playground.jsonl`). Used for legacy file-based session storage.
- **`--container <container_id>`**: Run extensions [inside a Docker container](/docs/tutorials/kaji-in-docker#running-extensions-in-docker-containers).
- **`--no-session`**: Run kaji commands without creating or storing a session file

**Extension Options:**
- **`--with-extension <COMMAND>`**: Add stdio extensions (can be used multiple times)
- **`--with-streamable-http-extension <URL>`**: Add remote extensions over Streamable HTTP (can be used multiple times)
- **`--with-builtin <name>`**: Add builtin extensions by name (e.g., 'developer' or multiple: 'developer,github')

**Control Options:**
- **`--debug`**: Output complete tool responses, detailed parameter values, and full file paths
- **`--max-tool-repetitions <NUMBER>`**: Maximum number of times the same tool can be called consecutively with identical parameters. Helps prevent infinite loops
- **`--max-turns <NUMBER>`**: Maximum number of turns allowed without user input (default: 1000)
- **`--explain`**: Show a recipe's title, description, and parameters
- **`--render-recipe`**: Print the rendered recipe instead of running it
- **`-q, --quiet`**: Quiet mode. Suppress non-response output, printing only the model response to stdout
- **`--output-format <FORMAT>`**: Output format (`text`, `json`, or `stream-json`). Default is `text`. Use JSON structured output for automation and scripting: `json` for results after completion, `stream-json` for events as they occur
- **`--provider`**: Specify the provider to use for this session (overrides environment variable)
- **`--model`**: Specify the model to use for this session (overrides environment variable)

**Usage:**
```bash
# Run from instruction file
kaji run --instructions plan.md

# Load a recipe with a prompt that kaji executes and then exits  
kaji run --recipe recipe.yaml

# Load a recipe and stay in an interactive session
kaji run --recipe recipe.yaml --interactive

# Load a recipe in debug mode
kaji run --recipe recipe.yaml --debug

# Show recipe details
kaji run --recipe recipe.yaml --explain

# Run a recipe with parameters
kaji run --recipe recipe.yaml --params environment=production --params region=us-west-2

# Run instructions from a file without session storage
kaji run --no-session -i instructions.txt

# Run with a specified provider and model
kaji run --provider anthropic --model claude-4-sonnet -t "initial prompt"

# Run with limited turns before prompting user
kaji run --recipe recipe.yaml --max-turns 10
```

---

#### review [options] [range]
Review the current git diff using kaji. By default, `kaji review` reviews the working tree against `HEAD`; pass a range such as `main...HEAD` to review a specific diff.

`kaji review` can discover review checks from `.agents/checks/*.md` and scoped review instructions from `.agents/REVIEW.md`.

**Options:**
- **`--prompt <FILE>`**: Use a custom base review prompt
- **`--model <MODEL>`**: Set the default model for the main review agent and checks that do not declare their own model
- **`--provider <PROVIDER>`**: Set the provider for the main review agent
- **`--override-model <MODEL>`**: Force every discovered check to use this model
- **`--turn-limit <N>`**: Set the default turn limit for orchestrated review subprocesses and checks
- **`--dry-run`**: Print the assembled review prompt and discovered checks without running the review
- **`-q, --quiet`**: Suppress non-result output from the underlying agent
- **`--no-orchestrate`**: Disable the default Rust-driven parallel orchestrator and use the single-prompt path
- **`-i, --instructions <TEXT>`**: Add free-form review instructions
- **`-f, --files <FILE>...`**: Restrict the review to specific files
- **`-c, --check-filter <NAME>...`**: Run only checks with matching names
- **`-s, --check-scope <DIR>`**: Search a specific directory for `.agents/checks/*.md`
- **`--checks-only`**: Skip the main correctness pass and run only check subagents
- **`--summary-only`**: Print only the diff summary
- **`--severity <LEVEL>`**: Minimum severity to display. Defaults to `medium`; use `low` to show every finding

**Usage:**
```bash
# Review the working tree against HEAD
kaji review

# Review a branch range
kaji review main...HEAD

# Add review intent
kaji review --instructions "This is a refactor; flag behavior changes"

# Review only selected files
kaji review --files crates/kaji/src/agents/agent.rs documentation/docs/guides/kaji-cli-commands.md

# Preview the assembled prompt and discovered checks
kaji review --dry-run

# Run only named checks
kaji review --check-filter security performance --checks-only
```

---

#### recipe
Used to validate recipe files, manage recipe sharing, list available recipes, and open recipes in kaji desktop.

**Commands:**
- **`deeplink <RECIPE_NAME>`**: Generate a shareable link for a recipe file
  - **`-p, --param <KEY=VALUE>`**: Pre-fill recipe parameter (can be specified multiple times)
- **`list [OPTIONS]`**: List all available recipes from local directories and configured GitHub repositories
  - **`--format <FORMAT>`**: Output format (`text` or `json`). Default is `text`
  - **`-v, --verbose`**: Show verbose information including recipe titles and full file paths
- **`open <RECIPE_NAME>`**: Open a recipe file directly in kaji desktop
  - **`-p, --param <KEY=VALUE>`**: Pre-fill recipe parameter (can be specified multiple times)
- **`validate <RECIPE_NAME>`**: Validate a recipe file

**Usage:**
```bash
# Generate a shareable link
kaji recipe deeplink my-recipe.yaml

# Generate a deeplink and provide parameter values
kaji recipe deeplink my-recipe.yaml -p environment=production -p region=us-west-2

# List all available recipes
kaji recipe list

# List recipes with detailed information
kaji recipe list --verbose

# List recipes in JSON format for automation
kaji recipe list --format json

# Open a recipe in kaji desktop
kaji recipe open my-recipe.yaml

# Open a recipe by name
kaji recipe open my-recipe

# Open a recipe and provide parameter value
kaji recipe open my-recipe --param name=myproject

# Validate a recipe file
kaji recipe validate my-recipe.yaml

# Get help about recipe commands
kaji recipe help
```

---

#### plugin
Install and update git-backed plugins that provide skills or other Open Plugins components.

**Commands:**
- **`install [OPTIONS] <URL>`**: Install a plugin from a git repository URL
  - **`--auto-update`**: Automatically check for updates before plugin skills are loaded
- **`update <NAME>`**: Update an installed git-backed plugin by name

**Usage:**
```bash
# Install a plugin from a git repository
kaji plugin install https://github.com/example/my-goose-plugin.git

# Install a plugin and enable automatic update checks
kaji plugin install --auto-update https://github.com/example/my-goose-plugin.git

# Update an installed plugin manually
kaji plugin update my-plugin
```

Installed plugins are stored under `~/.agents/plugins/<plugin-name>/`. For more about plugin-provided skills, hooks, and update behavior, see the [Plugins guide](/docs/guides/context-engineering/plugins).

---

#### skills
List skills available to the kaji agent.

**Commands:**
- **`list`**: List installed and discoverable skills, including token counts and source locations

**Usage:**
```bash
kaji skills list
```

---

#### local-models
Search, download, list, and delete local inference models.

:::info
This command is available in kaji builds that include local inference support.
:::

**Commands:**
- **`search <QUERY>`**: Search Hugging Face for compatible GGUF and MLX models
  - **`-l, --limit <NUMBER>`**: Maximum number of results to show. Defaults to `10`
- **`download <SPEC>`**: Download and register a model from a search result, such as `user/repo:Q4_K_M`
- **`list`**: List downloaded local models
- **`delete <ID>`**: Delete a downloaded local model

**Alias:** `lm`

**Usage:**
```bash
# Search for local models
kaji local-models search qwen --limit 5

# Download a model from a search result
kaji local-models download 'user/repo:Q4_K_M'

# List downloaded models
kaji local-models list

# Delete a downloaded model
kaji local-models delete user/repo:Q4_K_M
```

---

#### schedule
Automate recipes by running them on a [schedule](/docs/guides/recipes/session-recipes.md#schedule-recipe).

**Commands:**
- `add <OPTIONS>`: Create a new scheduled job. Copies the current version of the recipe to `~/.local/share/kaji/scheduled_recipes`
- `list`: View all scheduled jobs
- `remove`: Delete a scheduled job
- `sessions`: List sessions created by a scheduled recipe
- `run-now`: Run a scheduled recipe immediately
- `cron-help`: Show cron expression examples and help

**Options:**
- `--schedule-id <NAME>`: A unique ID for the scheduled job (e.g. `daily-report`)
- `--cron "* * * * * *"`: Specifies when a job should run using a [cron expression](https://en.wikipedia.org/wiki/Cron#Cron_expression)
- `--recipe-source <PATH>`: Path to the recipe YAML file
- `-l, --limit <NUMBER>`: Max number of sessions to display when using the `sessions` command

**Usage:**
```bash
kaji schedule <COMMAND>

# Add a new scheduled recipe which runs every day at 9 AM
kaji schedule add --schedule-id daily-report --cron "0 0 9 * * *" --recipe-source ./recipes/daily-report.yaml

# List all scheduled jobs
kaji schedule list

# List the 10 most recent kaji sessions created by a scheduled job
kaji schedule sessions --schedule-id daily-report -l 10

# Run a recipe immediately
kaji schedule run-now --schedule-id daily-report

# Remove a scheduled job
kaji schedule remove --schedule-id daily-report
```

---

#### mcp
Run an enabled MCP server specified by `<name>` (e.g. `'Google Drive'`).

**Usage:**
```bash
kaji mcp <name>
```

---

#### acp
Run kaji as an Agent Client Protocol (ACP) agent server over stdio. This enables kaji to work with ACP-compatible clients like Zed.

ACP is an emerging protocol specification that standardizes communication between AI agents and client applications, making it easier for clients to integrate with various AI agents.

**Options:**
- **`--enable-scheduler`**: Enable scheduled recipe execution. Disabled by default.

**Usage:**
```bash
kaji acp
```

:::info
This command is automatically invoked by ACP-compatible clients and is not typically run directly by users. The client manages the lifecycle of the `kaji acp` process. See [Using kaji in ACP Clients](/docs/guides/acp-clients) for details.
:::

---

#### serve [options]
Start kaji as an Agent Client Protocol (ACP) server over HTTP and WebSocket.

**Options:**
- **`--host <HOST>`**: Host to bind to. Defaults to `127.0.0.1`
- **`--port <PORT>`**: Port to listen on. Defaults to `3284`
- **`--with-builtin <NAME>`**: Enable built-in extensions by name. Can be passed multiple times or as a comma-separated list. Defaults to `developer` when omitted.
- **`--dangerously-unauthenticated`**: Run without ACP authentication. Use only for local trusted clients.
- **`--enable-scheduler`**: Enable scheduled recipe execution. Disabled by default.

**Usage:**
```bash
# Set a secret before starting the server
export KAJI_SERVER__SECRET_KEY=$(openssl rand -hex 32)

# Start the ACP server on localhost:3284
kaji serve

# Bind to a different host and port
kaji serve --host 0.0.0.0 --port 3284

# Start with specific built-in extensions
kaji serve --with-builtin developer
```

:::warning
`kaji serve` requires `KAJI_SERVER__SECRET_KEY` unless you pass `--dangerously-unauthenticated`. Only use `--dangerously-unauthenticated` with local trusted clients.
:::

---

### Terminal Integration

#### term
Set up and use terminal-integrated sessions. Terminal integration gives each shell a persistent kaji session through `AGENT_SESSION_ID`, and can create the `@kaji` and `@g` aliases.

**Commands:**
- **`init <SHELL>`** - Print the shell integration script for `bash`, `zsh`, `fish`, `nu`, or `powershell`
- **`run <PROMPT...>`** - Send a prompt to the terminal-integrated session
- **`info`** - Print compact session information for shell prompt integration

**Options:**
- **`-n, --name <NAME>`** - Set the terminal session name when running `init`
- **`--default`** - Ask kaji to handle unknown commands in supported shells

**Usage:**
```bash
# Set up zsh integration
eval "$(kaji term init zsh)"

# Set up zsh integration and ask kaji about unknown commands
eval "$(kaji term init zsh --default)"

# Set up nushell integration
let init = ($nu.cache-dir | path join "kaji-term-init.nu")
kaji term init nu | save --force $init
source $init

# Send a prompt to the current terminal session
kaji term run why did the last command fail

# Print session info for prompt integration
kaji term info
```

---

#### @kaji / @g
Ask kaji questions directly from your shell prompt, with command history included in the context. These aliases are created by `kaji term init` when you set up [terminal integration](/docs/guides/terminal-integration.md).

**Examples:**
```bash
# Ask questions with command history context
@kaji create a python script to process these files
@kaji create a PR description summarizing these changes
@g how do I fix these permission denied errors?
```

---

## Interactive Session Features

### Slash Commands

Once you're in an interactive session (via `kaji session` or `kaji run --interactive`), you can use these slash commands. All commands support tab completion. Press `/ + <Tab>` to cycle through available commands.

**Available Commands:**
- **`/?` or `/help`** - Display the help menu
- **`/builtin <names>`** - Add builtin extensions by name (comma-separated)
- **`/clear`** - Clear the current chat history
- **`/endplan`** - Exit plan mode and return to 'normal' kaji mode
- **`/exit` or `/quit`** - Exit the session
- **`/extension <command>`** - Add a stdio extension (format: ENV1=val1 command args...)
- **`/mode <name>`** - Set the kaji mode to use ('auto', 'approve', 'chat', 'smart_approve')
- **`/plan <message_text>`** - Enter 'plan' mode with optional message. Create a plan based on the current messages and ask user if they want to act on it
- **`/prompt <n> [--info] [key=value...]`** - Get prompt info or execute a prompt
- **`/prompts [--extension <name>]`** - List all available prompts, optionally filtered by extension
- **`/recipe [filepath]`** - Generate a recipe from the current conversation and save it to the specified filepath (must end with .yaml). If no filepath is provided, it will be saved to ./recipe.yaml
- **`/compact`** - Compact and summarize the current conversation to reduce context length while preserving key information
- **`/r`** - Toggle full tool output display (show complete tool parameters without truncation)
- **`/skills [<name>...]`** - List available skills, or load one or more skills by name
- **`/t`** - Toggle between `light`, `dark`, and `ansi` themes. [More info](#themes).
- **`/t <name>`** - Set theme directly (light, dark, ansi)

**Examples:**
```bash
# Create a plan for triaging test failures
/plan let's create a plan for triaging test failures

# List all prompts from the developer extension
/prompts --extension developer

# Switch to chat mode
/mode chat

# Add a builtin extension during the session
/builtin developer

# Clear the current conversation history
/clear
```
You can also create [custom slash commands for running recipes](/docs/guides/context-engineering/slash-commands) in kaji Desktop or the CLI. 

---

### Themes

The `/t` command controls the syntax highlighting theme for markdown content in kaji CLI responses. This affects the styles used for headers, code blocks, bold/italic text, and other markdown elements in the response output.

**Commands:**
- `/t` - Cycles through themes: `light` → `dark` → `ansi` → `light`
- `/t light` - Sets `light` theme (subtle light colors)
- `/t dark` - Sets `dark` theme (subtle darker colors)
- `/t ansi` - Sets `ansi` theme (most visually distinct option with brighter colors)

**Configuration:**
- The default theme is `dark`
- The theme setting is saved to the [configuration file](/docs/guides/config-files) as `KAJI_CLI_THEME` and persists between sessions
- The saved configuration can be overridden for the session using the `KAJI_CLI_THEME` [environment variable](/docs/guides/environment-variables#session-management)

**Custom Syntax Highlighting:**

You can customize the underlying syntax highlighting theme used for code blocks by setting:
- `KAJI_CLI_LIGHT_THEME` - Theme used when in light mode (default: "GitHub")
- `KAJI_CLI_DARK_THEME` - Theme used when in dark mode (default: "zenburn")

These accept any [bat theme name](https://github.com/sharkdp/bat#adding-new-themes). Popular options include "Dracula", "Nord", "Solarized (light)", "Solarized (dark)", "OneHalfDark", and "Monokai Extended". Run `bat --list-themes` to see all available themes.

:::info
Syntax highlighting styles only affect the font, not the overall terminal interface. The `light` and `dark` themes have subtle differences in font color and weight.

The kaji CLI theme is independent from the kaji Desktop theme.
:::

**Examples:**
```bash
# Set ANSI theme for the session via environment variable
export KAJI_CLI_THEME=ansi
kaji session --name use-custom-theme

# Toggle theme during a session
/t

# Set the light theme during a session
/t light
```

---

## Navigation and Controls

### Keyboard Shortcuts

**Session Control:**
- **`Ctrl+C`** - Clear the current line if text is entered, interrupt the current request if processing, or exit the session if line is empty
- **`Ctrl+J`** - Add a newline. Can customize the character via `KAJI_CLI_NEWLINE_KEY` in the [config file](/docs/guides/config-files) (e.g. `KAJI_CLI_NEWLINE_KEY: n`) or as an [environment variable](/docs/guides/environment-variables#session-management). Avoid "c" and common terminal shortcuts like "r", "w", "z".

**Navigation:**
- **`Cmd+Up/Down arrows`** - Navigate through command history
- **`Ctrl+R`** - Interactive command history search (reverse search). [More info](#command-history-search).

---

### External Editor Mode

For composing longer prompts or working with complex code snippets, you can configure kaji to use your preferred text editor instead of CLI input. This replaces the standard CLI input and keyboard shortcuts for the entire session.

**How it works:**
1. kaji opens your configured editor with a template file
2. Type your prompt after the `# Your prompt:` heading (conversation history is shown below for context)
3. Save the file and close/exit the editor to send your prompt to kaji
4. kaji processes your prompt and reopens the editor with the response added to the conversation history
5. Repeat steps 2-4 for each message in the conversation

You can use any editor that accepts a file path argument, such as vim, nano, emacs, and VS Code.

**Configuration:**

<Tabs>
  <TabItem value="envvar" label="Environment Variable" default>

  Applies to the current session only.

  ```bash
  # For terminal editors like vim or nano
  export KAJI_PROMPT_EDITOR=vim

  # Or for GUI editors like VS Code (use --wait flag)
  export KAJI_PROMPT_EDITOR="code --wait"
  ```

  </TabItem>
  <TabItem value="config" label="Config File">

  Persists across all sessions unless overridden by the environment variable.
  
  1. Navigate to the kaji [configuration file](/docs/guides/config-files). For example, navigate to `~/.config/kaji/config.yaml` on macOS.
  2. Add `KAJI_PROMPT_EDITOR` and set it to your preferred editor:
  
  ```yaml
  # For terminal editors like vim or nano
  KAJI_PROMPT_EDITOR: vim

  # Or for GUI editors like VS Code (use --wait flag)
  KAJI_PROMPT_EDITOR: code --wait
  ```

  </TabItem>
</Tabs>

**Using GUI Editors:**

GUI editors require a `--wait` or equivalent flag to ensure kaji waits for you to finish editing before continuing. Without this flag, the editor opens but kaji immediately proceeds as if you're done. Terminal editors like vim and nano don't need this flag.

---

### Command History Search

The `Ctrl+R` shortcut provides interactive search through your stored CLI [command history](/docs/guides/logs#command-history). This feature makes it easy to find and reuse recent commands without retyping them. When you type a search term, kaji searches backwards through your history for matches.

**How it works:**
1. Press `Ctrl+R` in your kaji CLI session
2. Type a search term
3. Navigate through the results using:
   - `Ctrl+R` to cycle backwards through earlier matches
   - `Ctrl+S` to cycle forward through newer matches
4. Press `Return` (or `Enter`) to run the found command, or `Esc` to cancel

For example, instead of retyping this long command:

```
analyze the performance issues in the sales database queries and suggest optimizations
```

Use the `"sales database"` or `"optimization"` search term to find and rerun it.

**Search tips:**
- **Distinctive terms work best**: Choose unique words or phrases to help filter the results
- **Partial matches and multiple words are supported**: You can search for phrases like `"gith"` and `"run the unit test"`
