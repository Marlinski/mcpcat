//! mcpcat — a "netcat for MCP".
//!
//! Inspect and drive any Model Context Protocol server from the shell:
//! list/call tools, list/read resources, list/get prompts, or drop into an
//! interactive shell. Talks to servers over **stdio** (launch a command) or
//! **Streamable HTTP** (a URL), built on the official Rust SDK, `rmcp`.

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, GetPromptRequestParams, RawContent, ReadResourceRequestParams,
};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A running client connection to an MCP server.
type Client = RunningService<RoleClient, ()>;

#[derive(Parser)]
#[command(
    name = "mcpcat",
    version,
    about = "netcat for MCP — inspect & call MCP servers over stdio or Streamable HTTP",
    long_about = "mcpcat connects to an MCP server and lets you list/call tools, \
                  list/read resources, list/get prompts, or open an interactive shell.\n\n\
                  The SERVER is either:\n  \
                  • a URL  (http(s)://host/path)        → Streamable HTTP transport\n  \
                  • a command + args                    → stdio transport (spawned)\n\n\
                  Options (like -p) must come BEFORE the SERVER spec.\n\n\
                  If SERVER is omitted, it is read from the MCPCAT_SERVER \
                  environment variable, so you can:\n  \
                  export MCPCAT_SERVER=http://localhost:3001/mcp\n  \
                  mcpcat tools          # and shell, call, resources, … just work\n\n\
                  Examples:\n  \
                  mcpcat tools http://localhost:3001/mcp\n  \
                  mcpcat call echo -p '{\"message\":\"hi\"}' http://localhost:3001/mcp\n  \
                  mcpcat tools docker exec -i mcp-everything mcp-server-everything stdio\n  \
                  mcpcat shell http://localhost:3001/mcp"
)]
struct Cli {
    /// Output format.
    #[arg(short, long, value_enum, default_value_t = Format::Pretty, global = true)]
    format: Format,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Format {
    /// Human-readable.
    Pretty,
    /// Raw JSON (pipe to `jq`).
    Json,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the tools a server exposes.
    Tools {
        #[command(flatten)]
        server: ServerSpec,
    },
    /// Call a tool.
    Call {
        /// Tool name.
        tool: String,
        /// Arguments as a JSON object.
        #[arg(short, long, default_value = "{}")]
        params: String,
        #[command(flatten)]
        server: ServerSpec,
    },
    /// List resources.
    Resources {
        #[command(flatten)]
        server: ServerSpec,
    },
    /// Read a resource by URI.
    Read {
        /// Resource URI.
        uri: String,
        #[command(flatten)]
        server: ServerSpec,
    },
    /// List prompts.
    Prompts {
        #[command(flatten)]
        server: ServerSpec,
    },
    /// Get a prompt by name.
    Prompt {
        /// Prompt name.
        name: String,
        /// Arguments as a JSON object.
        #[arg(short, long, default_value = "{}")]
        params: String,
        #[command(flatten)]
        server: ServerSpec,
    },
    /// Open an interactive shell against a server (one persistent connection).
    Shell {
        #[command(flatten)]
        server: ServerSpec,
    },
}

#[derive(Args, Clone)]
struct ServerSpec {
    /// URL (Streamable HTTP) or a command + args (stdio). Must come last.
    /// If omitted, falls back to the MCPCAT_SERVER environment variable.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 0..,
        value_name = "SERVER"
    )]
    server: Vec<String>,
}

impl ServerSpec {
    /// Resolve the server spec: explicit args win; otherwise fall back to the
    /// `MCPCAT_SERVER` env var (whitespace-split, so it can hold a stdio
    /// command line as well as a URL).
    fn resolve(&self) -> Result<Vec<String>> {
        if !self.server.is_empty() {
            return Ok(self.server.clone());
        }
        match std::env::var("MCPCAT_SERVER") {
            Ok(s) if !s.trim().is_empty() => {
                Ok(s.split_whitespace().map(String::from).collect())
            }
            _ => bail!(
                "no server specified: pass a SERVER (a URL or a command) \
                 or set the MCPCAT_SERVER environment variable"
            ),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let fmt = cli.format;

    match cli.cmd {
        Cmd::Tools { server } => {
            let client = connect(&server).await?;
            list_tools(&client, fmt).await?;
            client.cancel().await.ok();
        }
        Cmd::Call { tool, params, server } => {
            let client = connect(&server).await?;
            call_tool(&client, &tool, &params, fmt).await?;
            client.cancel().await.ok();
        }
        Cmd::Resources { server } => {
            let client = connect(&server).await?;
            list_resources(&client, fmt).await?;
            client.cancel().await.ok();
        }
        Cmd::Read { uri, server } => {
            let client = connect(&server).await?;
            read_resource(&client, &uri, fmt).await?;
            client.cancel().await.ok();
        }
        Cmd::Prompts { server } => {
            let client = connect(&server).await?;
            list_prompts(&client, fmt).await?;
            client.cancel().await.ok();
        }
        Cmd::Prompt { name, params, server } => {
            let client = connect(&server).await?;
            get_prompt(&client, &name, &params, fmt).await?;
            client.cancel().await.ok();
        }
        Cmd::Shell { server } => {
            let client = connect(&server).await?;
            run_shell(&client, fmt).await?;
            client.cancel().await.ok();
        }
    }
    Ok(())
}

/// Connect to a server: a URL → Streamable HTTP, anything else → stdio command.
async fn connect(spec: &ServerSpec) -> Result<Client> {
    let server = spec.resolve()?;
    let head = &server[0];
    if head.starts_with("http://") || head.starts_with("https://") {
        let transport = StreamableHttpClientTransport::from_uri(head.clone());
        ()
            .serve(transport)
            .await
            .with_context(|| format!("failed to connect to {head} (Streamable HTTP)"))
    } else {
        let mut cmd = tokio::process::Command::new(head);
        cmd.args(&server[1..]);
        let transport = TokioChildProcess::new(cmd)
            .with_context(|| format!("failed to spawn server process `{head}`"))?;
        ()
            .serve(transport)
            .await
            .with_context(|| format!("failed to connect to stdio server `{head}`"))
    }
}

// ─── Operations ───────────────────────────────────────────────────────────────

async fn list_tools(client: &Client, fmt: Format) -> Result<()> {
    let tools = client.list_all_tools().await.context("list_tools failed")?;
    match fmt {
        Format::Json => println!("{}", serde_json::to_string_pretty(&tools)?),
        Format::Pretty => {
            if tools.is_empty() {
                println!("(no tools)");
            }
            for t in &tools {
                let args = schema_arg_summary(&t.input_schema);
                println!("\x1b[1m{}\x1b[0m({})", t.name, args);
                if let Some(d) = &t.description {
                    println!("    {}", first_line(d));
                }
            }
        }
    }
    Ok(())
}

async fn call_tool(client: &Client, tool: &str, params: &str, fmt: Format) -> Result<()> {
    let mut req = CallToolRequestParams::new(tool.to_string());
    req.arguments = parse_args_object(params)?;
    let result = client
        .call_tool(req)
        .await
        .with_context(|| format!("call_tool `{tool}` failed"))?;
    match fmt {
        Format::Json => println!("{}", serde_json::to_string_pretty(&result)?),
        Format::Pretty => {
            if result.is_error.unwrap_or(false) {
                eprintln!("\x1b[31m[tool reported an error]\x1b[0m");
            }
            for c in &result.content {
                print_content(&c.raw);
            }
            if let Some(sc) = &result.structured_content {
                println!("{}", serde_json::to_string_pretty(sc)?);
            }
        }
    }
    Ok(())
}

async fn list_resources(client: &Client, fmt: Format) -> Result<()> {
    let resources = client.list_all_resources().await.context("list_resources failed")?;
    match fmt {
        Format::Json => println!("{}", serde_json::to_string_pretty(&resources)?),
        Format::Pretty => {
            if resources.is_empty() {
                println!("(no resources)");
            }
            for r in &resources {
                println!("\x1b[1m{}\x1b[0m  \x1b[2m{}\x1b[0m", r.name, r.uri);
                if let Some(d) = &r.description {
                    println!("    {}", first_line(d));
                }
            }
        }
    }
    Ok(())
}

async fn read_resource(client: &Client, uri: &str, fmt: Format) -> Result<()> {
    let result = client
        .read_resource(ReadResourceRequestParams::new(uri.to_string()))
        .await
        .with_context(|| format!("read_resource `{uri}` failed"))?;
    match fmt {
        Format::Json => println!("{}", serde_json::to_string_pretty(&result)?),
        Format::Pretty => println!("{}", serde_json::to_string_pretty(&result.contents)?),
    }
    Ok(())
}

async fn list_prompts(client: &Client, fmt: Format) -> Result<()> {
    let prompts = client.list_all_prompts().await.context("list_prompts failed")?;
    match fmt {
        Format::Json => println!("{}", serde_json::to_string_pretty(&prompts)?),
        Format::Pretty => {
            if prompts.is_empty() {
                println!("(no prompts)");
            }
            for p in &prompts {
                let args = p
                    .arguments
                    .as_ref()
                    .map(|a| a.iter().map(|x| x.name.clone()).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default();
                println!("\x1b[1m{}\x1b[0m({})", p.name, args);
                if let Some(d) = &p.description {
                    println!("    {}", first_line(d));
                }
            }
        }
    }
    Ok(())
}

async fn get_prompt(client: &Client, name: &str, params: &str, fmt: Format) -> Result<()> {
    let mut req = GetPromptRequestParams::new(name.to_string());
    req.arguments = parse_args_object(params)?;
    let result = client
        .get_prompt(req)
        .await
        .with_context(|| format!("get_prompt `{name}` failed"))?;
    match fmt {
        Format::Json => println!("{}", serde_json::to_string_pretty(&result)?),
        Format::Pretty => println!("{}", serde_json::to_string_pretty(&result.messages)?),
    }
    Ok(())
}

// ─── Interactive shell ──────────────────────────────────────────────────────

async fn run_shell(client: &Client, fmt: Format) -> Result<()> {
    if let Some(info) = client.peer_info() {
        println!(
            "Connected to \x1b[1m{}\x1b[0m v{}",
            info.server_info.name, info.server_info.version
        );
    }
    println!("Type 'help' for commands, 'exit' to quit.");

    let mut out = tokio::io::stdout();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    out.write_all(b"\x1b[1mmcpcat>\x1b[0m ").await?;
    out.flush().await?;

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if !line.is_empty() {
            let (cmd, rest) = split_first(line);
            // Per-command errors should not kill the shell.
            if let Err(e) = dispatch_shell(client, cmd, rest, fmt).await {
                eprintln!("\x1b[31merror:\x1b[0m {e:#}");
            }
            if matches!(cmd, "exit" | "quit") {
                break;
            }
        }
        out.write_all(b"\x1b[1mmcpcat>\x1b[0m ").await?;
        out.flush().await?;
    }
    Ok(())
}

async fn dispatch_shell(client: &Client, cmd: &str, rest: &str, fmt: Format) -> Result<()> {
    match cmd {
        "help" | "?" => {
            println!(
                "commands:\n  \
                 tools                         list tools\n  \
                 call <tool> [json-args]       call a tool (args default to {{}})\n  \
                 resources                     list resources\n  \
                 read <uri>                    read a resource\n  \
                 prompts                       list prompts\n  \
                 prompt <name> [json-args]     get a prompt\n  \
                 info                          show server info\n  \
                 help | ?                      this help\n  \
                 exit | quit                   disconnect and leave"
            );
        }
        "tools" => list_tools(client, fmt).await?,
        "resources" => list_resources(client, fmt).await?,
        "prompts" => list_prompts(client, fmt).await?,
        "info" => {
            if let Some(info) = client.peer_info() {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                println!("(no server info)");
            }
        }
        "call" => {
            let (tool, args) = split_first(rest);
            if tool.is_empty() {
                bail!("usage: call <tool> [json-args]");
            }
            let args = if args.trim().is_empty() { "{}" } else { args };
            call_tool(client, tool, args, fmt).await?;
        }
        "read" => {
            let uri = rest.trim();
            if uri.is_empty() {
                bail!("usage: read <uri>");
            }
            read_resource(client, uri, fmt).await?;
        }
        "prompt" => {
            let (name, args) = split_first(rest);
            if name.is_empty() {
                bail!("usage: prompt <name> [json-args]");
            }
            let args = if args.trim().is_empty() { "{}" } else { args };
            get_prompt(client, name, args, fmt).await?;
        }
        "exit" | "quit" => {}
        other => bail!("unknown command: {other} (try 'help')"),
    }
    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Parse a `--params`/shell JSON string into the optional argument object an
/// MCP call expects. `{}` / empty → `None`; a JSON object → `Some(map)`.
fn parse_args_object(s: &str) -> Result<Option<serde_json::Map<String, Value>>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(s).context("arguments must be valid JSON")?;
    match v {
        Value::Object(m) if m.is_empty() => Ok(None),
        Value::Object(m) => Ok(Some(m)),
        Value::Null => Ok(None),
        _ => bail!("arguments must be a JSON object, e.g. '{{\"key\":\"value\"}}'"),
    }
}

/// Compact one-line summary of a tool's input schema: `name:type, [optional:type]`.
fn schema_arg_summary(schema: &serde_json::Map<String, Value>) -> String {
    let Some(Value::Object(props)) = schema.get("properties") else {
        return String::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    props
        .iter()
        .map(|(name, spec)| {
            let ty = spec.get("type").and_then(|t| t.as_str()).unwrap_or("any");
            let ty = short_type(ty);
            if required.contains(&name.as_str()) {
                format!("{name}:{ty}")
            } else {
                format!("[{name}:{ty}]")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn short_type(t: &str) -> &str {
    match t {
        "string" => "str",
        "number" => "num",
        "integer" => "int",
        "boolean" => "bool",
        "object" => "obj",
        "array" => "arr",
        other => other,
    }
}

fn print_content(c: &RawContent) {
    match c {
        RawContent::Text(t) => println!("{}", t.text),
        RawContent::Image(i) => println!("[image: {} ({} bytes b64)]", i.mime_type, i.data.len()),
        RawContent::Audio(a) => println!("[audio: {} ({} bytes b64)]", a.mime_type, a.data.len()),
        RawContent::Resource(r) => {
            println!("[embedded resource] {}", serde_json::to_string(&r.resource).unwrap_or_default())
        }
        RawContent::ResourceLink(r) => println!("[resource link] {} ({})", r.name, r.uri),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Split a string into (first whitespace-delimited token, remainder).
fn split_first(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}
