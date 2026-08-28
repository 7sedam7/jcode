# Herdr integration contract

Jcode has built-in terminal routing for Herdr. When a headed session launch is requested from a client with `HERDR_ENV=1` and `HERDR_PANE_ID`, Jcode splits the calling pane to the right, focuses the new pane, and starts the resumed Jcode session there. `HERDR_BIN_PATH` is honored when present.

This covers visible swarm spawns, resume-in-new-terminal, self-development launches, and restart restores because they all use the shared terminal launcher. A configured `[terminal].spawn_hook` still takes precedence.

## Current compatibility

Jcode already:

- forwards `HERDR_ENV`, `HERDR_SOCKET_PATH`, `HERDR_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`, `HERDR_BIN_PATH`, `HERDR_SESSION`, and `HERDR_AGENT` from the requesting client to server-side spawn and focus paths;
- recognizes Herdr as a masking terminal multiplexer for Mermaid graphics capability detection;
- reports pane-scoped `idle`, `working`, and `blocked` lifecycle state through Herdr's public CLI when `HERDR_ENV=1` and `HERDR_PANE_ID` are present, using `HERDR_BIN_PATH` when supplied and otherwise resolving `herdr` from `PATH`;
- attaches the active Jcode session ID to each lifecycle report and releases its reporting authority when the foreground Jcode process exits;
- reports `blocked` for session-scoped ambient approvals and commands waiting for terminal input;
- exports stable lifecycle observer hooks for `session_start`, `session_end`, `turn_start`, and `turn_end`;
- exports `JCODE_HOOK_SESSION_ID`, `JCODE_HOOK_CWD`, event fields, and a JSON `JCODE_HOOK_PAYLOAD`;
- resumes a native session with `jcode --resume <session-id>`.

## Built-in lifecycle reporting

The foreground Jcode process owns lifecycle reporting because Herdr pane identity belongs to that process rather than the shared Jcode daemon. The daemon may serve several panes and sessions concurrently, so it must not publish pane-scoped state from its inherited environment.

Jcode invokes the Herdr binary asynchronously with reports equivalent to:

```text
herdr pane report-agent <pane-id> \
  --source custom:jcode \
  --agent jcode \
  --state working \
  --agent-session-id <jcode-session-id>
```

Reports are serialized by one dedicated worker, deduplicated, sequence-ordered, kept in a small bounded queue, retried after transient failures, and time-bounded. Approval observation also runs off the TUI thread. Reporting never blocks model streaming or terminal input, and process-exit cleanup has its own bounded wait. State maps as follows:

- awaiting a prompt or a completed turn: `idle`;
- sending, generating, streaming, or running a tool: `working`;
- pending ambient approval or a command waiting for stdin: `blocked`, with a concise message;
- foreground process exit: `pane release-agent`.

Herdr derives an unseen done state from the `working` to `idle` transition, and uses `blocked` for needs-input attention and notifications. Jcode therefore does not send duplicate notification calls.

Jcode reports its own stable session ID, not an upstream provider conversation ID. A future first-class Herdr restore adapter can resume it with:

```text
jcode --resume <agent_session_id>
```

Jcode session IDs are opaque strings and fit Herdr's ID-based session reference model. No transcript path is needed.

## Required Herdr-side work

A first-class integration cannot be shipped only as a remote detection manifest. Herdr currently hard-codes known agent kinds, official session sources, restore commands, and install targets. The upstream implementation needs:

1. Add `jcode` to `IntegrationTarget`, CLI parsing, labels, command discovery, recommendations, status, install, and uninstall handling.
2. Install a config-safe Jcode session hook adapter without overwriting an existing user hook. If Herdr cannot safely compose the single Jcode hook command, coordinate a small multi-hook or native-emitter addition in Jcode first.
3. Accept `("herdr:jcode", "jcode")` as an official session source.
4. Persist its ID session reference and map it to `jcode --resume <id>` during restore.
5. Add Jcode process detection and a bundled screen manifest for idle, working, and blocked UI states.
6. Keep screen-manifest detection authoritative until Jcode exposes complete blocked, approval-result, interrupt, and exit transitions.
7. Add integration versioning, replacement-source handling, schema/UI wiring, install/uninstall tests, restore-plan tests, detection fixtures, and documentation.

Relevant upstream files as of Herdr commit `eacea2daf0b72973173b728936b27478374f2cd2`:

- `src/integration/{mod.rs,registry.rs,targets.rs,actions.rs,version.rs}`
- `src/integration/assets/`
- `src/api/schema/integrations.rs`
- `src/agent_resume.rs`
- `src/detect/mod.rs`
- `src/terminal/state.rs`

## Future lifecycle coverage

The built-in reporter covers Jcode's current structured user-decision paths. If Jcode adds another modal question or approval protocol, that protocol must also feed the foreground reporter before it is considered complete lifecycle coverage. Herdr screen detection can remain a compatibility fallback for older Jcode versions.

Official references:

- <https://herdr.dev/docs/integrations/>
- <https://herdr.dev/docs/socket-api/>
- <https://herdr.dev/docs/agents/>
- <https://herdr.dev/docs/session-state/>
