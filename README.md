# deos-mcpd

**Governance proxy for Model Context Protocol (MCP) servers.**

Every `tools/call` gets a content-addressed **permit** before it runs and a
**receipt** after it returns. Append-only audit trail. Zero changes to the
upstream MCP server. One-line swap in your Claude Desktop / Claude Code config.

Part of the [DEOS](https://github.com/DEOS-Computing) governance stack.

## Why

MCP is great at connecting Claude to tools. It is silent about who authorized
a given call and what actually happened. `deos-mcpd` sits between the MCP
client and server, forwards JSON-RPC untouched, and emits a tamper-evident
permit/receipt pair for every governed action.

- **Permit** — emitted before the call. Binds `{session, tool, args_hash, request_id, ts}` with a BLAKE3 id.
- **Receipt** — emitted after the response. References the permit id + result hash + status + duration.
- **Chain** — receipts point at permits; permits are content-addressed; the JSONL file is append-only.

## Install

```bash
git clone https://github.com/DEOS-Computing/deos-mcpd
cd deos-mcpd
cargo install --path .
```

Requires Rust 1.75+. Binary lands in `~/.cargo/bin/deos-mcpd`.

## Use

### Wrap any stdio MCP server

Before:

```json
"filesystem": {
  "command": "npx",
  "args": ["-y", "@modelcontextprotocol/server-filesystem", "/Users/you/Documents"]
}
```

After:

```json
"filesystem": {
  "command": "deos-mcpd",
  "args": [
    "--upstream", "npx", "-y", "@modelcontextprotocol/server-filesystem", "/Users/you/Documents"
  ]
}
```

Claude Desktop config lives at
`~/Library/Application Support/Claude/claude_desktop_config.json`.
Restart Claude Desktop after editing.

Receipts land in `~/.deos-mcpd/receipts.jsonl`. Tail it to watch governed
activity in real time:

```bash
tail -f ~/.deos-mcpd/receipts.jsonl | jq .
```

### Sample output

```json
{"kind":"permit","id":"9f8c…","session_id":"d3a1…","upstream":"npx -y @modelcontextprotocol/server-filesystem /Users/you/Documents","tool_name":"read_file","args_hash":"c5d2…","request_id":"7","timestamp_ms":1713570102331}
{"kind":"receipt","id":"a177…","permit_id":"9f8c…","session_id":"d3a1…","status":"ok","result_hash":"b804…","duration_ms":4,"timestamp_ms":1713570102335}
```

## Status

- [x] stdio transport
- [x] permit / receipt emission for `tools/call`
- [x] JSONL append-only log
- [x] Session ID threading
- [ ] HTTP + SSE transport (Week 2)
- [ ] Policy DSL: `tool: github.delete_repo → require_approval` (Week 2)
- [ ] Hosted dashboard + team tier (Week 3)
- [ ] Enterprise: DEOS kernel as receipt store + replay (Week 4)

## Design notes

- Wire bytes are never mutated. We parse a copy of each line; the original is
  forwarded verbatim. If inspection fails for any reason, the byte stream
  continues to flow.
- Request/response pairing is by JSON-RPC `id`. MCP servers must echo the id
  per spec; if they don't, we drop the receipt and never block the call.
- BLAKE3 hashing is over `serde_json::to_vec` output, which is deterministic
  for a given `Value` tree. For a fully canonical content-address we'd want
  key-sorted JSON — on the roadmap.
- The proxy is intentionally dumb about which tools are dangerous. That's the
  policy layer's job (Week 2).

## License

MIT
