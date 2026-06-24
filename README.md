# mcpcat

A **netcat for MCP** — inspect and drive any [Model Context Protocol](https://modelcontextprotocol.io)
server straight from your shell. List and call tools, list and read resources,
list and get prompts, or drop into an interactive shell.

Built on the official Rust SDK, [`rmcp`](https://crates.io/crates/rmcp).

Unlike some MCP CLIs, mcpcat speaks **Streamable HTTP** (the modern transport),
as well as **stdio** (spawning a server process).

## Install

```sh
cargo install --path .
# or, after building:
cargo build --release   # binary at target/release/mcpcat
```

## Transports

The `SERVER` argument selects the transport automatically:

| `SERVER` looks like…            | Transport        |
| ------------------------------- | ---------------- |
| `http://…` / `https://…`        | Streamable HTTP  |
| anything else (a command + args)| stdio (spawned)  |

> Options such as `-p` must come **before** the `SERVER` spec.

## Usage

```sh
# List tools
mcpcat tools http://localhost:3001/mcp

# Call a tool (arguments as a JSON object)
mcpcat call echo -p '{"message":"hi"}' http://localhost:3001/mcp

# Talk to a stdio server (everything after the subcommand is the command line)
mcpcat tools npx -y @modelcontextprotocol/server-everything stdio
mcpcat tools docker exec -i mcp-everything mcp-server-everything stdio

# Resources & prompts
mcpcat resources http://localhost:3001/mcp
mcpcat read   "test://static/resource/1" http://localhost:3001/mcp
mcpcat prompts http://localhost:3001/mcp
mcpcat prompt  simple_prompt http://localhost:3001/mcp

# Raw JSON output (pipe to jq)
mcpcat -f json tools http://localhost:3001/mcp | jq '.[].name'

# Interactive shell — one persistent connection
mcpcat shell http://localhost:3001/mcp
```

### Shell commands

```
tools                       list tools
call <tool> [json-args]     call a tool (args default to {})
resources                   list resources
read <uri>                  read a resource
prompts                     list prompts
prompt <name> [json-args]   get a prompt
info                        show server info
help | ?                    help
exit | quit                 disconnect and leave
```

## License

MIT
