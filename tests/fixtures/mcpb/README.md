`mcpb-manifest-v0.3.schema.json` is the MCP Bundle manifest schema, version
0.3, vendored unchanged from
<https://github.com/modelcontextprotocol/mcpb/blob/70fe3b34cd6dff1b3bba046638edc72a6467a4fb/schemas/mcpb-manifest-v0.3.schema.json>
(fetched 7 November 2026, SHA-256
`3a0ac9d845711a1b9b17dfa5a52f8b60628239d6a86a9db417206a9efc78592d`).
`tests/suite/agent_docs.rs` validates the generated
`scripts/release/mcpb-manifest.json` against it offline. When
`guide::MCPB_MANIFEST_VERSION` moves to a newer version, replace this file
with that version's schema.
