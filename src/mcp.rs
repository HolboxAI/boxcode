//! Minimal MCP (Model Context Protocol) client.
//!
//! Why this is hand-rolled rather than built on the `rmcp` crate: the MCP stdio
//! transport is newline-delimited JSON-RPC 2.0, which is the same framing
//! `transport.rs` already speaks to the editor on the other side of this
//! process. Adding `rmcp` would pull a large dependency tree into a crate that
//! deliberately keeps its dependency list short, and it would have to be
//! fetched at build time. The subset needed here - initialize, tools/list,
//! tools/call - is implemented directly against the `serde_json` and `tokio`
//! dependencies that are already present.
//!
//! Scope: this is the client half. It connects a configured server, enumerates
//! its tools and calls them. It does not decide *whether* a call is permitted;
//! that stays with the approval path.
//!
//! NOTE: `#![allow(dead_code)]` is temporary - this module is not wired into
//! `transport.rs` yet, so nothing calls it. Remove it when it is.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Protocol revisions this client will speak, newest first. The server replies
/// with the revision it intends to use; a reply outside this list is rejected
/// rather than assumed compatible.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2026-07-28", "2025-11-25"];

/// How long to wait for a server to answer one request. A server that hangs
/// must not hang the agent, so every request is bounded.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Prefix for tool ids surfaced to the model, e.g. `mcp__github__create_issue`.
/// The separator is deliberately not a single `_`, so a server named `a` with a
/// tool named `b__c` cannot collide with a server named `a__b` with tool `c`.
pub const TOOL_PREFIX: &str = "mcp";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpServerConfig {
	#[serde(rename = "stdio")]
	Stdio {
		name: String,
		command: String,
		#[serde(default)]
		args: Vec<String>,
		#[serde(default, deserialize_with = "de_key_values")]
		env: BTreeMap<String, String>,
	},
	#[serde(rename = "streamable-http")]
	Http {
		name: String,
		url: String,
		#[serde(default, deserialize_with = "de_key_values")]
		headers: BTreeMap<String, String>,
	},
}

/// Accepts `env`/`headers` in either shape seen on the wire: a JSON object
/// (`{"NAME":"value"}`) or the ordered array the extension sends
/// (`[{"name":"NAME","value":"value"}]`). Rejecting a plausible variant
/// silently is worse than accepting both.
fn de_key_values<'de, D>(de: D) -> Result<BTreeMap<String, String>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	#[derive(serde::Deserialize)]
	struct Entry {
		name: String,
		value: String,
	}

	#[derive(serde::Deserialize)]
	#[serde(untagged)]
	enum Both {
		Map(BTreeMap<String, String>),
		List(Vec<Entry>),
	}

	Ok(match Both::deserialize(de)? {
		Both::Map(map) => map,
		Both::List(list) => list.into_iter().map(|e| (e.name, e.value)).collect(),
	})
}

impl McpServerConfig {
	pub fn name(&self) -> &str {
		match self {
			McpServerConfig::Stdio { name, .. } => name,
			McpServerConfig::Http { name, .. } => name,
		}
	}

	/// Reject configurations that cannot work before anything is spawned.
	pub fn validate(&self) -> Result<(), String> {
		match self {
			McpServerConfig::Stdio { name, command, .. } => {
				if name.trim().is_empty() {
					return Err("stdio server has an empty name".into());
				}
				if command.trim().is_empty() {
					return Err(format!("stdio server '{name}' has an empty command"));
				}
				Ok(())
			}
			McpServerConfig::Http { name, url, .. } => {
				if name.trim().is_empty() {
					return Err("http server has an empty name".into());
				}
				if !(url.starts_with("http://") || url.starts_with("https://")) {
					return Err(format!("http server '{name}' has a url that is not http(s)"));
				}
				Ok(())
			}
		}
	}
}

/// Turn a server or tool name into a fragment safe for a model-facing tool id.
/// Anything outside `[A-Za-z0-9_-]` becomes `_`.
fn slug(raw: &str) -> String {
	let mut out = String::with_capacity(raw.len());
	for ch in raw.chars() {
		if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
			out.push(ch);
		} else {
			out.push('_');
		}
	}
	if out.is_empty() {
		out.push('_');
	}
	out
}

/// The id fragment for a server name.
///
/// Underscores are deliberately not allowed to survive here. The separator is a
/// double underscore, so a server fragment containing `_` makes the boundary
/// between server and tool ambiguous: `mcp__a__b__c` could come either from
/// server "a" with tool "b__c", or from server "a__b" with tool "c". Those are
/// different servers, so a permission granted for one would silently apply to
/// the other. Mapping `_` to `-` keeps the fragment underscore-free, which makes
/// splitting on the first `__` unambiguous. Two distinct server names that map
/// onto the same fragment are rejected when the config is parsed, so they cannot
/// collapse onto one id either.
pub fn server_id_fragment(server: &str) -> String {
	slug(server).replace('_', "-")
}

/// The model-facing id for a tool on a server.
pub fn tool_id(server: &str, tool: &str) -> String {
	format!(
		"{TOOL_PREFIX}__{}__{}",
		server_id_fragment(server),
		slug(tool)
	)
}

/// The inverse of [`tool_id`]: `"mcp__github__create_issue"` -> server
/// `"github"`, tool `"create_issue"`.
///
/// Splitting on the *first* `__` after the prefix is unambiguous because
/// `server_id_fragment` maps every `_` to `-`, so a server fragment can never
/// contain the separator. Two server names that would collapse onto the same
/// fragment are rejected when the config is parsed, so a name that reaches here
/// has a well-defined origin.
///
/// A tool name may itself contain `__`, which is why only the first one splits.
pub fn parse_tool_id(id: &str) -> Option<(&str, &str)> {
	let rest = id.strip_prefix(TOOL_PREFIX)?.strip_prefix("__")?;
	let (server, tool) = rest.split_once("__")?;
	if server.is_empty() || tool.is_empty() {
		return None;
	}
	Some((server, tool))
}

/// Parse the `mcpServers` array as it arrives from the editor.
///
/// The ACP schema makes `type` optional and defaults to stdio, and accepts
/// `http` as a synonym for `streamable-http`; both are normalised here so the
/// serde representation can stay strict.
pub fn parse_server_configs(raw: &[Value]) -> Result<Vec<McpServerConfig>, String> {
	let mut out = Vec::with_capacity(raw.len());
	for (idx, entry) in raw.iter().enumerate() {
		let mut obj = entry
			.as_object()
			.cloned()
			.ok_or_else(|| format!("mcpServers[{idx}] is not an object"))?;

		let transport = obj
			.get("type")
			.and_then(Value::as_str)
			.unwrap_or("stdio")
			.to_ascii_lowercase();
		let transport = match transport.as_str() {
			"http" => "streamable-http",
			other => other,
		};
		obj.insert("type".into(), Value::String(transport.to_string()));

		let cfg: McpServerConfig = serde_json::from_value(Value::Object(obj))
			.map_err(|e| format!("mcpServers[{idx}] is not a valid server config: {e}"))?;
		cfg.validate()?;
		out.push(cfg);
	}

	// Distinct servers must not collapse onto the same id, or a permission
	// granted for one would silently apply to another.
	let mut seen: BTreeSet<String> = BTreeSet::new();
	for cfg in &out {
		let key = server_id_fragment(cfg.name());
		if !seen.insert(key.clone()) {
			return Err(format!(
				"two mcpServers entries collide on the id '{key}'; rename one"
			));
		}
	}

	Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTool {
	pub name: String,
	pub description: Option<String>,
	pub input_schema: Value,
}

/// A tool as it should be presented to the model, carrying both the namespaced
/// id the model calls and the server-local name the server expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolDescriptor {
	pub id: String,
	pub server: String,
	pub tool: String,
	pub description: Option<String>,
	pub input_schema: Value,
}

/// A connected MCP server.
pub struct McpClient {
	server: String,
	child: Child,
	stdin: ChildStdin,
	stdout: BufReader<ChildStdout>,
	next_id: i64,
	protocol_version: String,
}

impl McpClient {
	pub fn server_name(&self) -> &str {
		&self.server
	}

	pub fn protocol_version(&self) -> &str {
		&self.protocol_version
	}

	/// Spawn a stdio server and complete the initialize handshake.
	pub async fn connect(config: &McpServerConfig) -> Result<Self, String> {
		config.validate()?;
		let McpServerConfig::Stdio {
			name,
			command,
			args,
			env,
		} = config
		else {
			return Err("streamable-http servers are not implemented yet".into());
		};

		let mut cmd = Command::new(command);
		cmd.args(args)
			.envs(env)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			// A server that outlives the agent would keep a granted process
			// alive; killing on drop is the conservative default.
			.kill_on_drop(true);

		let mut child = cmd
			.spawn()
			.map_err(|e| format!("could not start mcp server '{name}' ({command}): {e}"))?;

		let stdin = child
			.stdin
			.take()
			.ok_or_else(|| format!("mcp server '{name}' has no stdin"))?;
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| format!("mcp server '{name}' has no stdout"))?;

		// Drain stderr. Not doing so lets a chatty server fill the pipe buffer
		// and block forever, which would look like a hang in the handshake.
		if let Some(stderr) = child.stderr.take() {
			let label = name.clone();
			tokio::spawn(async move {
				let mut lines = BufReader::new(stderr).lines();
				while let Ok(Some(line)) = lines.next_line().await {
					tracing::debug!(target: "mcp", server = %label, "{}", line);
				}
			});
		}

		let mut client = McpClient {
			server: name.clone(),
			child,
			stdin,
			stdout: BufReader::new(stdout),
			next_id: 1,
			protocol_version: String::new(),
		};

		let result = client.initialize().await?;
		let version = result
			.get("protocolVersion")
			.and_then(Value::as_str)
			.unwrap_or("")
			.to_string();
		if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version.as_str()) {
			return Err(format!(
				"mcp server '{name}' answered with unsupported protocol version '{version}'"
			));
		}
		client.protocol_version = version;
		Ok(client)
	}

	async fn initialize(&mut self) -> Result<Value, String> {
		let params = json!({
			"protocolVersion": SUPPORTED_PROTOCOL_VERSIONS[0],
			"capabilities": {},
			"clientInfo": { "name": "boxcode", "version": env!("CARGO_PKG_VERSION") },
		});
		self.request("initialize", params, DEFAULT_TIMEOUT).await
	}

	pub async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
		let result = self
			.request("tools/list", json!({}), DEFAULT_TIMEOUT)
			.await?;
		let tools = result
			.get("tools")
			.and_then(Value::as_array)
			.ok_or_else(|| format!("mcp server '{}' returned no tools array", self.server))?;

		let mut out = Vec::with_capacity(tools.len());
		for tool in tools {
			let name = tool
				.get("name")
				.and_then(Value::as_str)
				.ok_or_else(|| format!("mcp server '{}' returned a tool with no name", self.server))?
				.to_string();
			out.push(McpTool {
				name,
				description: tool
					.get("description")
					.and_then(Value::as_str)
					.map(str::to_string),
				input_schema: tool
					.get("inputSchema")
					.cloned()
					.unwrap_or_else(|| json!({ "type": "object" })),
			});
		}
		Ok(out)
	}

	/// Call a tool by its server-local name and flatten the text content blocks
	/// into a single string. A tool that reports `isError` is an error here.
	pub async fn call_tool(&mut self, tool: &str, args: Value) -> Result<String, String> {
		let result = self
			.request(
				"tools/call",
				json!({ "name": tool, "arguments": args }),
				DEFAULT_TIMEOUT,
			)
			.await?;
		let is_error = result
			.get("isError")
			.and_then(Value::as_bool)
			.unwrap_or(false);

		let mut text = String::new();
		if let Some(blocks) = result.get("content").and_then(Value::as_array) {
			for block in blocks {
				if let Some(t) = block.get("text").and_then(Value::as_str) {
					if !text.is_empty() {
						text.push('\n');
					}
					text.push_str(t);
				}
			}
		}

		if is_error {
			return Err(if text.is_empty() {
				format!("mcp tool '{tool}' failed")
			} else {
				text
			});
		}
		Ok(text)
	}

	async fn request(
		&mut self,
		method: &str,
		params: Value,
		timeout: Duration,
	) -> Result<Value, String> {
		let id = self.next_id;
		self.next_id += 1;

		let payload = json!({
			"jsonrpc": "2.0",
			"id": id,
			"method": method,
			"params": params,
		});
		let mut encoded = serde_json::to_vec(&payload)
			.map_err(|e| format!("could not encode {method}: {e}"))?;
		encoded.push(b'\n');

		self.stdin
			.write_all(&encoded)
			.await
			.map_err(|e| format!("could not write {method} to '{}': {e}", self.server))?;
		self.stdin
			.flush()
			.await
			.map_err(|e| format!("could not flush {method} to '{}': {e}", self.server))?;

		let server = self.server.clone();
		let read = async {
			loop {
				let mut line = String::new();
				let n = self
					.stdout
					.read_line(&mut line)
					.await
					.map_err(|e| format!("could not read from '{server}': {e}"))?;
				if n == 0 {
					return Err(format!("mcp server '{server}' closed its output during {method}"));
				}
				let trimmed = line.trim();
				if trimmed.is_empty() {
					continue;
				}
				let msg: Value = match serde_json::from_str(trimmed) {
					Ok(v) => v,
					// A server may log to stdout; ignore anything that is not JSON.
					Err(_) => continue,
				};
				// Skip notifications and responses to other requests.
				if msg.get("id").and_then(Value::as_i64) != Some(id) {
					continue;
				}
				if let Some(err) = msg.get("error") {
					let message = err
						.get("message")
						.and_then(Value::as_str)
						.unwrap_or("unknown error");
					return Err(format!("mcp server '{server}' rejected {method}: {message}"));
				}
				return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
			}
		};

		match tokio::time::timeout(timeout, read).await {
			Ok(result) => result,
			Err(_) => Err(format!(
				"mcp server '{}' did not answer {method} within {}s",
				self.server,
				timeout.as_secs()
			)),
		}
	}
}

/// Build the model-facing descriptors for one connected server.
pub fn descriptors_for(server: &str, tools: &[McpTool]) -> Vec<McpToolDescriptor> {
	tools
		.iter()
		.map(|t| McpToolDescriptor {
			id: tool_id(server, &t.name),
			server: server.to_string(),
			tool: t.name.clone(),
			description: t.description.clone(),
			input_schema: t.input_schema.clone(),
		})
		.collect()
}

/// Every MCP server connected for one session, with the tools each advertises.
///
/// Owned by the session actor task, so a connection lives exactly as long as its
/// session and dies with it. Tools are listed once, here, and must never be
/// re-listed: the model-facing schema list has to stay byte-identical across the
/// rounds of a turn or the prefix cache is busted (see `tools::schemas_for`).
#[derive(Default)]
pub struct McpRegistry {
	servers: Vec<ConnectedServer>,
}

struct ConnectedServer {
	name: String,
	/// Held so `call_tool` can reach it later. Unused until dispatch lands.
	client: McpClient,
	tools: Vec<McpToolDescriptor>,
}

impl McpRegistry {
	/// The client for the server a tool id names.
	///
	/// `None` when no connected server answers to that name, which is the
	/// honest answer for an id naming a server this session never reached,
	/// rather than an error the caller has to decode.
	pub fn client_mut(&mut self, server: &str) -> Option<&mut McpClient> {
		self.servers
			.iter_mut()
			.find(|s| s.name == server)
			.map(|s| &mut s.client)
	}

	/// Connects every configured server and lists the tools it offers.
	///
	/// A server that cannot be reached is reported and skipped, never fatal: one
	/// bad entry should not cost you the working ones, and a session with no MCP
	/// tools is still a usable session. The caller gets the problems so it can
	/// surface them rather than leave a silent hole.
	///
	/// Must not be called on the request path -- the handshake spawns a process
	/// and can take seconds, so `session/new` replies first and this runs after.
	pub async fn connect_all(configs: &[McpServerConfig]) -> (Self, Vec<String>) {
		let mut registry = Self::default();
		let mut problems = Vec::new();

		for config in configs {
			let name = config.name().to_string();
			let mut client = match McpClient::connect(config).await {
				Ok(client) => client,
				Err(e) => {
					problems.push(format!("{name}: could not connect: {e}"));
					continue;
				}
			};
			match client.list_tools().await {
				Ok(tools) => {
					let tools = descriptors_for(&name, &tools);
					registry.servers.push(ConnectedServer { name, client, tools });
				}
				Err(e) => problems.push(format!("{name}: could not list tools: {e}")),
			}
		}

		(registry, problems)
	}

	pub fn is_empty(&self) -> bool {
		self.servers.is_empty()
	}

	/// Every connected server's name, in the order they were configured.
	pub fn server_names(&self) -> impl Iterator<Item = &str> {
		self.servers.iter().map(|s| s.name.as_str())
	}

	/// Every tool across every server, in the shape the model is shown.
	pub fn descriptors(&self) -> impl Iterator<Item = &McpToolDescriptor> {
		self.servers.iter().flat_map(|s| s.tools.iter())
	}

	pub fn tool_count(&self) -> usize {
		self.servers.iter().map(|s| s.tools.len()).sum()
	}
}


#[cfg(test)]
mod tests {

	#[test]
	fn env_accepts_the_ordered_array_shape_the_extension_sends() {
		let raw = serde_json::json!([{
			"type": "stdio",
			"name": "demo",
			"command": "npx",
			"env": [
				{ "name": "TOKEN", "value": "abc" },
				{ "name": "MODE", "value": "dev" }
			]
		}]);
		let parsed = parse_server_configs(raw.as_array().expect("json array")).expect("array-shaped env must be accepted");
		match &parsed[0] {
			McpServerConfig::Stdio { env, .. } => {
				assert_eq!(env.get("TOKEN").map(String::as_str), Some("abc"));
				assert_eq!(env.get("MODE").map(String::as_str), Some("dev"));
			}
			other => panic!("expected a stdio config, got {other:?}"),
		}
	}

	#[test]
	fn env_still_accepts_a_plain_object() {
		let raw = serde_json::json!([{
			"type": "stdio",
			"name": "demo",
			"command": "npx",
			"env": { "TOKEN": "abc" }
		}]);
		let parsed = parse_server_configs(raw.as_array().expect("json array")).expect("object-shaped env must be accepted");
		match &parsed[0] {
			McpServerConfig::Stdio { env, .. } => {
				assert_eq!(env.get("TOKEN").map(String::as_str), Some("abc"));
			}
			other => panic!("expected a stdio config, got {other:?}"),
		}
	}

	#[test]
	fn headers_accept_the_ordered_array_shape_too() {
		let raw = serde_json::json!([{
			"type": "streamable-http",
			"name": "demo",
			"url": "https://example.invalid/mcp",
			"headers": [{ "name": "Authorization", "value": "Bearer x" }]
		}]);
		let parsed = parse_server_configs(raw.as_array().expect("json array")).expect("array-shaped headers must be accepted");
		match &parsed[0] {
			McpServerConfig::Http { headers, .. } => {
				assert_eq!(
					headers.get("Authorization").map(String::as_str),
					Some("Bearer x")
				);
			}
			other => panic!("expected an http config, got {other:?}"),
		}
	}
	use super::*;

	#[test]
	fn stdio_is_the_default_transport() {
		let raw = json!([{ "name": "filesystem", "command": "npx", "args": ["-y", "fs"] }]);
		let parsed = parse_server_configs(raw.as_array().unwrap()).unwrap();
		assert_eq!(parsed.len(), 1);
		match &parsed[0] {
			McpServerConfig::Stdio { name, command, args, .. } => {
				assert_eq!(name, "filesystem");
				assert_eq!(command, "npx");
				assert_eq!(args.len(), 2);
			}
			other => panic!("expected stdio, got {other:?}"),
		}
	}

	#[test]
	fn http_is_accepted_as_an_alias_for_streamable_http() {
		let raw = json!([{ "type": "http", "name": "remote", "url": "https://example.test/mcp" }]);
		let parsed = parse_server_configs(raw.as_array().unwrap()).unwrap();
		assert!(matches!(parsed[0], McpServerConfig::Http { .. }));
	}

	#[test]
	fn a_stdio_server_without_a_command_is_rejected() {
		let raw = json!([{ "name": "broken", "command": "   " }]);
		let err = parse_server_configs(raw.as_array().unwrap()).unwrap_err();
		assert!(err.contains("empty command"), "unexpected error: {err}");
	}

	#[test]
	fn a_non_http_url_is_rejected() {
		let raw = json!([{ "type": "streamable-http", "name": "bad", "url": "ftp://x" }]);
		let err = parse_server_configs(raw.as_array().unwrap()).unwrap_err();
		assert!(err.contains("not http"), "unexpected error: {err}");
	}

	#[test]
	fn colliding_server_ids_are_rejected_rather_than_merged() {
		// Both names slug to "a_b", so a permission stored against one id would
		// otherwise apply to the other.
		let raw = json!([
			{ "name": "a b", "command": "one" },
			{ "name": "a_b", "command": "two" }
		]);
		let err = parse_server_configs(raw.as_array().unwrap()).unwrap_err();
		assert!(err.contains("collide"), "unexpected error: {err}");
	}

	#[test]
	fn tool_ids_are_namespaced_and_sanitised() {
		assert_eq!(tool_id("github", "create_issue"), "mcp__github__create_issue");
		assert_eq!(tool_id("my server", "do.it"), "mcp__my-server__do_it");
	}

	#[test]
	fn tool_ids_cannot_collide_across_the_separator() {
		// server "a" tool "b__c" must not equal server "a__b" tool "c".
		assert_ne!(tool_id("a", "b__c"), tool_id("a__b", "c"));
	}

	#[test]
	fn descriptors_carry_both_the_namespaced_id_and_the_local_name() {
		let tools = vec![McpTool {
			name: "create_issue".into(),
			description: Some("Open an issue".into()),
			input_schema: json!({ "type": "object" }),
		}];
		let d = descriptors_for("github", &tools);
		assert_eq!(d[0].id, "mcp__github__create_issue");
		assert_eq!(d[0].tool, "create_issue");
		assert_eq!(d[0].server, "github");
	}

	#[test]
	fn we_only_advertise_versions_we_can_actually_speak() {
		assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&"2026-07-28"));
		assert!(!SUPPORTED_PROTOCOL_VERSIONS.is_empty());
	}
	fn fake_server_bin() -> std::path::PathBuf {
		let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
		p.push("target");
		p.push(if cfg!(debug_assertions) { "debug" } else { "release" });
		p.push(if cfg!(windows) {
			"mcp-fake-server.exe"
		} else {
			"mcp-fake-server"
		});
		p
	}

	fn stdio_config(name: &str, env: serde_json::Value) -> McpServerConfig {
		let raw = serde_json::json!([{
			"type": "stdio",
			"name": name,
			"command": fake_server_bin().to_string_lossy(),
			"args": [],
			"env": env,
		}]);
		let mut parsed = parse_server_configs(raw.as_array().expect("json array"))
			.expect("fake server config should parse");
		parsed.remove(0)
	}

	/// The gap this closes: until now nothing ever spoke to a server, so the
	/// framing and handshake were compile-verified only.
	#[tokio::test]
	async fn connects_to_a_stdio_server_and_lists_its_tools() {
		let bin = fake_server_bin();
		assert!(bin.exists(), "cargo test builds bins first; missing {}", bin.display());

		let (registry, problems) = McpRegistry::connect_all(&[stdio_config("fake", serde_json::json!({}))]).await;

		assert!(problems.is_empty(), "unexpected connect problems: {problems:?}");
		let names: Vec<&str> = registry.server_names().collect();
		assert!(names.contains(&"fake"), "server not registered: {names:?}");
		assert_eq!(registry.tool_count(), 2, "the fake advertises echo,add");
		let described = format!("{:?}", registry.descriptors().collect::<Vec<_>>());
		assert!(described.contains("echo"), "tool list did not reach the client: {described}");
		assert!(described.contains("add"), "tool list did not reach the client: {described}");
	}

	/// A server that fails to handshake is reported and skipped, never fatal.
	#[tokio::test]
	async fn a_server_that_fails_initialize_is_reported_not_fatal() {
		let mut bad = stdio_config("broken", serde_json::json!({}));
		match &mut bad {
			McpServerConfig::Stdio { env, .. } => {
				env.insert("MCP_FAKE_FAIL_INIT".to_string(), "1".to_string());
			}
			other => panic!("expected a stdio config, got {other:?}"),
		}

		let (registry, problems) = McpRegistry::connect_all(&[bad, stdio_config("good", serde_json::json!({}))]).await;

		assert_eq!(problems.len(), 1, "the broken server must be reported: {problems:?}");
		let names = format!("{:?}", registry.server_names().collect::<Vec<_>>());
		assert!(names.contains("good"), "the healthy server must still connect: {names}");
		assert!(!names.contains("broken"), "a failed server must not be registered: {names}");
	}

	/// The env/headers wire mismatch shipped once because nothing checked that
	/// configured env actually reaches the child process. This does.
	#[tokio::test]
	async fn configured_env_reaches_the_child_process() {
		let config = stdio_config(
			"fake",
			serde_json::json!({
				"MCP_FAKE_ECHO_ENV": "PROBE_TOKEN",
				"PROBE_TOKEN": "wire-works"
			}),
		);

		let (registry, problems) = McpRegistry::connect_all(&[config]).await;

		assert!(problems.is_empty(), "{problems:?}");
		let described = format!("{:?}", registry.descriptors().collect::<Vec<_>>());
		assert!(described.contains("wire-works"), "config env never reached the child: {described}");
	}


	#[test]
	fn parse_tool_id_is_the_inverse_of_tool_id() {
		for (server, tool) in [("github", "create_issue"), ("aws", "s3__get_object"), ("srv-with-dash", "a")] {
			let id = tool_id(server, tool);
			assert_eq!(parse_tool_id(&id), Some((server, tool)), "round trip for {id}");
		}
	}

	#[test]
	fn parse_tool_id_rejects_names_that_are_not_mcp_tools() {
		for id in ["", "mcp", "mcp_github_create_issue", "mcp__", "mcp__github__", "github__create_issue", "read_file"] {
			assert_eq!(parse_tool_id(id), None, "{id} should not parse");
		}
	}

}
