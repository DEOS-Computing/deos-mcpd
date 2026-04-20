# deos-mcpd

**Governance proxy for Model Context Protocol (MCP) servers.**

Every `tools/call` gets a content-addressed **permit** before it runs and a
**receipt** after it returns. Append-only audit trail. Zero changes to the
upstream MCP server. One-line swap in your Claude Desktop / Claude Code config.

Part of the [DEOS](https://github.com/DEOS-Computing) governance stack.

## Demo

Wrap the filesystem MCP server, make a tool call, watch the audit trail emerge:

```
$ deos-mcpd --upstream npx -y @modelcontextprotocol/server-filesystem /tmp/demo &
$ # … Claude makes a tools/call for read_text_file …
$ tail -f ~/.deos-mcpd/receipts.jsonl | jq -c .

{"kind":"permit","id":"f350ca4d…","session_id":"d3a1…","tool_name":"read_text_file","args_hash":"db04c382…","request_id":"3","timestamp_ms":1776658150176}
{"kind":"receipt","id":"c6d67506…","permit_id":"f350ca4d…","status":"ok","result_hash":"654ee2a8…","duration_ms":916}
```

The permit is emitted **before** the call hits the upstream server. The receipt
is emitted **after** the response comes back. Both are content-addressed; the
receipt's `permit_id` is the permit's BLAKE3 id. The upstream server sees a
byte-identical JSON-RPC stream — no behavioral change to the tool itself.

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

## Policy + approval (v0.2.0)

Point the proxy at a YAML policy to go from *logger* to *governor*:

```bash
deos-mcpd --policy ./policy.yaml \
  --upstream npx -y @modelcontextprotocol/server-github
```

Example `policy.yaml`:

```yaml
version: 1
default: allow

rules:
  - id: block-github-destructive
    tool: "delete_repository|force_push|delete_branch"
    tool_regex: true
    action: deny

  - id: approve-fs-writes
    tool: "write_file|edit_file|move_file|create_directory"
    tool_regex: true
    action: require_approval
    approval_timeout_s: 300

  - id: fs-reads-inside-project
    tool: read_text_file
    args:
      path:
        not_starts_with: "/Users/you/projects"
    action: deny
```

Three actions:
- `allow` — permit + forward + receipt (the v0.1 path)
- `deny` — block, synthesize a JSON-RPC `-32001` error back to the client, emit a **denial** record. Upstream never sees the call.
- `require_approval` — hold the call, emit `approval_requested`, wait on the control API, then either forward or synthesize an error depending on the decision.

### Dashboard + approval control API (v0.3.0)

The same process that runs the proxy also serves a local dashboard +
JSON control API at `http://127.0.0.1:4005` (override with
`--control host:port`, or disable with `--control off`).

Open `http://127.0.0.1:4005/` in a browser to see:

- session list (per-session permit / receipt / denial / approval counts)
- live record timeline with kind chips and status/duration
- pending approvals inbox — click Approve / Deny to release a held call

No npm install. The dashboard is a single HTML page embedded directly
into the Rust binary. It auto-refreshes every 2 seconds.

Programmatic endpoints (same port):

```bash
# pending approvals awaiting decision
curl -s http://127.0.0.1:4005/pending | jq .

# session summaries
curl -s http://127.0.0.1:4005/api/sessions | jq .

# records (supports ?session=… and ?kind=…)
curl -s 'http://127.0.0.1:4005/api/records?session=<id>&kind=denial' | jq .

# decide a pending approval
curl -X POST http://127.0.0.1:4005/approve/<id>
curl -X POST http://127.0.0.1:4005/deny/<id>
```

Timeouts are per-rule (`approval_timeout_s`, default 120). A timed-out
approval emits a `denial` and synthesizes the same `-32001` error back
to the MCP client.

## Status

- [x] stdio transport
- [x] permit / receipt / denial / approval records
- [x] YAML policy DSL (`allow` / `deny` / `require_approval`)
- [x] Approval control API (`GET /pending`, `POST /approve/:id`, `POST /deny/:id`)
- [x] Local dashboard at `http://127.0.0.1:4005/` (single-file, zero deps)
- [x] Read API: `GET /api/sessions`, `GET /api/records?session=&kind=&limit=`
- [x] JSONL append-only log
- [x] Session ID threading
- [ ] HTTP + SSE transport (Week 4)
- [ ] Hosted receiver + team tier (Week 4)
- [ ] Enterprise: DEOS kernel as receipt store + replay (Week 5+)

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
