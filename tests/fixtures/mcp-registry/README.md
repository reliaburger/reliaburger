`server.schema.json` is the MCP Registry's server.json schema, vendored
unchanged from
<https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json>
(fetched 6 November 2026, SHA-256
`3fba09590c99f61735d234822279f4223fab9e300c0a81e81c91ab62a4114de0`).
`tests/suite/agent_docs.rs` validates the generated `server.json` against it
offline. When `guide::MCP_SERVER_SCHEMA` moves to a newer schema, replace this
file with that one.
