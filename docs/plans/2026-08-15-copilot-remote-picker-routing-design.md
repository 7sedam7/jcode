# Copilot Remote Picker Routing Design

## Problem

The server's initial `History` payload sends aggregate model names but
deliberately omits route metadata. The connection then records the provider's
full routed snapshot as already delivered, even though those routes were never
sent. Attach-time prefetch also returns early whenever any model names exist.
As a result, the client may never receive the authoritative provider routes.

The remote TUI compensates by inferring providers from model names. That is not
valid for aggregate catalogs: Copilot intentionally exposes upstream IDs such
as `gpt-5.6-sol`, which the heuristic classifies as OpenAI. The picker therefore
shows an unavailable OpenAI row instead of the valid Copilot route. An explicit
`/model copilot:gpt-5.6-sol` fuzzy-matches that incorrect row, and the picker
rejects it before the canonical command can reach the server.

This behavior was reproduced with an actual Linux pseudo-terminal and real
daemon, so it is not macOS-specific.

## Design

### Preserve route identity at the server boundary

The server must track what it actually sent. A names-only `History` payload
must not seed the connection dedup state with the unsent full routed snapshot.
After History, the connection must deliver an `AvailableModelsUpdated` event
containing authoritative routes. If the full event is too large, the existing
compact representation retains model, provider, API method, and availability
while dropping optional detail and pricing metadata.

Attach-time model refresh must consider both names and routes. Existing model
names are not sufficient evidence that the catalog is hydrated. When routes
are missing, prefetch must run; when routes already exist but were omitted from
History, they must still be published to the client.

### Do not infer provider ownership from aggregate names

Names-only remote snapshots must not assign provider ownership using model
family heuristics. Missing routes are represented as neutral remote-catalog
placeholders while the authoritative routed update is requested. This prevents
GPT, Claude, and Gemini IDs from being incorrectly attributed to OpenAI,
Anthropic, or Gemini when the server actually offers them through Copilot.

No model-specific Copilot list or special case for Sol will be added.

### Canonical commands remain authoritative

A complete canonical command such as `/model copilot:gpt-5.6-sol` must not be
blocked by an unavailable or placeholder fuzzy match. Available authoritative
picker rows still use structured `SetRoute`; otherwise the explicit command
falls through to `SetModel`, where the server's canonical parser and provider
runtime decide whether it is valid.

## Testing

Regression coverage must verify:

1. Names-only History does not mark unsent routes as delivered.
2. Attach with model names but missing routes triggers authoritative route
   delivery.
3. Aggregate names-only fallback never classifies `gpt-5.6-sol` as OpenAI.
4. A full terminal event for `/model copilot:gpt-5.6-sol` reaches the server
   even when only a placeholder or unavailable fuzzy row exists.
5. A real server/client transport receives an available Copilot Sol route,
   switches the server to Sol, and serves the next prompt with Sol.
