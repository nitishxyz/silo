# otto-client

`Connection` multiplexes RPC responses and server events. Subscribe to `events()`
before sending `session.subscribe`, since replay may precede its acknowledgement.
`SessionFollower` maintains a `SessionMirror`, reconnects local transports and
resumes from the last durable sequence.

Typed helpers include `session_create` (`SessionCreateOptions.effort`),
`session_update(id, SessionUpdate { provider, model, agent, effort, reset_effort })` and
`project_list()` (most recent first).

## Durable events and live overlays

`SessionMirror::apply(seq, event)` accepts only the next journal sequence; duplicate
sequences do not mutate state, and gaps require replay or a fresh snapshot.
`apply_live(after_seq, event)` accepts only an anchor equal to `last_seq`, without
advancing that cursor. Stale or future display messages can be dropped safely.
Only text, reasoning, tool-input, and tool-progress events are allowed live.

Live data is a provisional overlay on a saved durable item/tool projection. Before
applying each journal event, the mirror restores that projection, applies the event,
then reapplies outstanding live data. A coalesced durable text/input delta consumes
matching live prefixes, including partial chunks; it never appends the same text
twice. If a live prefix is missing because its lossy stream lagged, durable data
wins. `item.done` replaces all parts and discards the item's pending overlay. Tool
completion and turn completion also discard corresponding ephemeral state.

`clear_live()` restores the durable display when replacing a transport. Followers
call it before resubscribing; replay alone is enough to rebuild the same final
items. Live-only display, persisted-only replay, and a mixture therefore converge
at authoritative item completion.

The additive protocol-v1 live envelope has `live: true`, `id: 0`, `session_id`,
`after_seq`, and a nested tagged `Event`, but no `seq`. The zero ID makes older
clients parse it as an unsolicited response and ignore it without disconnecting.
New clients route it to `apply_live`; it is never passed to journal advancement.
