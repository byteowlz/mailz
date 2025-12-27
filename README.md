# mailz

Agent coordination via mail-style messaging, with CLI, TUI, MCP, and HTTP API interfaces.

## Overview

mailz keeps lightweight, auditable coordination data in a local SQLite database. Agents can send
messages, request acknowledgements, and reserve files to avoid conflicts. Operators can manage
projects and agents from a CLI or TUI, and integrate via HTTP API or MCP.

## Install

Build from source:

```bash
cargo build --release
```

## Quick Start

Initialize config and register an agent:

```bash
cargo run -p mailz-cli -- init
cargo run -p mailz-cli -- project create /path/to/repo --name repo
cargo run -p mailz-cli -- agent register repo --name "AgentA" --program "cli" --model "gpt"
```

Send a message and check inbox:

```bash
cargo run -p mailz-cli -- send repo --from AgentA --to AgentB --subject "Heads up" --body "Working on parser."
cargo run -p mailz-cli -- inbox repo --agent AgentB
```

Start the TUI:

```bash
cargo run -p mailz-tui
```

Start the API server:

```bash
cargo run -p mailz-api -- --port 3000
```

Start the MCP server:

```bash
cargo run -p mailz-mcp
```

## CLI Examples

Register agents and send messages:

```bash
cargo run -p mailz-cli -- agent register repo --name "AgentB" --program "cursor" --model "gpt"
cargo run -p mailz-cli -- send repo --from AgentA --to AgentB --subject "Review" --body "Please review parser changes."
```

Reserve files for coordinated edits:

```bash
cargo run -p mailz-cli -- reserve repo --agent AgentA src/parser.rs --ttl 1200 --reason "Refactor"
cargo run -p mailz-cli -- reservations repo
```

Watch for new messages:

```bash
cargo run -p mailz-cli -- watch
cargo run -p mailz-cli -- daemon start
cargo run -p mailz-cli -- daemon status
cargo run -p mailz-cli -- daemon stop
```

## TUI Basics

- `j/k` or arrows to navigate, `c` to compose, `t` for reservations, `g` for directory.
- `/` to search inbox, `r` to toggle read, `a` to acknowledge.
- Directory view shows projects (Tab to switch) and agents with unread/total counts.

## API Examples

Admin key is required for management endpoints. Set `api.admin_key` in config or read the auto
generated key from the state directory.

Create a project and agent:

```bash
curl -H "X-API-Key: $MAILZ_ADMIN_KEY" \
  -H "Content-Type: application/json" \
  -d '{"path":"/path/to/repo","name":"repo"}' \
  http://localhost:3000/projects

curl -H "X-API-Key: $MAILZ_ADMIN_KEY" \
  -H "Content-Type: application/json" \
  -d '{"name":"AgentA","program":"api","model":"gpt"}' \
  http://localhost:3000/projects/1/agents
```

Issue an agent key and use it to read inbox:

```bash
curl -H "X-API-Key: $MAILZ_ADMIN_KEY" http://localhost:3000/agents/1/keys

curl -H "X-API-Key: $MAILZ_AGENT_KEY" http://localhost:3000/agents/1/inbox
```

Connect to WebSocket updates (requires `websocat`):

```bash
websocat -H "X-API-Key: $MAILZ_AGENT_KEY" \
  ws://localhost:3000/ws/1
```

## MCP Examples

Run the MCP server and call tools such as:

- `ensure_project`
- `register_agent`
- `send_message`
- `fetch_inbox`
- `reserve_files`

## Configuration Reference

Default config path: `$XDG_CONFIG_HOME/mailz/config.toml`

Key sections (see `examples/config.toml`):

- `logging.level`, `logging.file`
- `runtime.parallelism`, `runtime.timeout`, `runtime.fail_fast`
- `paths.data_dir`, `paths.state_dir`
- `maintenance.gc_interval_seconds`, `maintenance.message_retention_days`
- `api.admin_key`, `api.rate_limit_per_minute`
- `tui.poll_interval_seconds`
- `watch.poll_interval_seconds`

## Agent Coordination Patterns

- Create one project per repo/workspace and register each agent/tool once.
- Use `reserve` before editing shared files; release when done.
- Mark read/acknowledge important messages to keep the inbox clean.
- Use threads (`thread_id`) to group related discussions.
- Keep agent names consistent across CLI, API, and MCP for clear audit trails.
