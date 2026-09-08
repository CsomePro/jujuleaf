# JujuLeaf Bridge Protocol

`jujuleaf bridge` is the stable process boundary for programs that integrate
with JujuLeaf. It hides private Overleaf HTTP, Socket.IO, ranges, history-OT,
and cache representations behind a versioned JSON protocol.

Protocol version 1 is read-only and covers comment discovery, authoritative
thread context, and comment wake-up events.

## Transport rules

- One-shot commands write exactly one compact JSON value followed by `\n`.
- `comments watch` writes one compact JSON value per line (NDJSON).
- stdout never contains ANSI escapes, progress bars, or human notices.
- Normal operational failures are JSON on stdout followed by a non-zero exit.
- stderr is not part of the protocol and is reserved for optional diagnostics.
- `--raw` and `--pretty` are rejected for bridge commands.
- Callers must ignore unknown optional fields.

Run bridge commands inside a JujuLeaf clone to use its project and profile. An
external caller can instead pass global `--project-id` and `--profile` options:

```console
jujuleaf --project-id PROJECT_ID --profile NAME bridge comments list --protocol 1
```

## Discovery

```console
jujuleaf bridge describe
```

`describe` does not require authentication or a project. Its `data` reports the
binary version, supported bridge versions, operations, normalized event types,
and stable error codes. Consumers should select a mutually supported protocol
version rather than comparing JujuLeaf SemVer strings.

## Common envelope

A successful one-shot response has this shape:

```json
{
  "protocol": {"name": "jujuleaf.bridge", "version": 1},
  "kind": "result",
  "operation": "comments.get",
  "ok": true,
  "data": {}
}
```

Stream records additionally contain one process-local session ID and a strictly
increasing sequence number:

```json
{
  "protocol": {"name": "jujuleaf.bridge", "version": 1},
  "kind": "comment.event",
  "operation": "comments.watch",
  "ok": true,
  "sessionId": "uuid",
  "sequence": 4,
  "data": {}
}
```

The sequence is not a durable Overleaf cursor. A new process or reconnect can
only establish authority through a full `comments.snapshot`.

## Comment snapshots

```console
jujuleaf bridge comments list --protocol 1
```

`comments.list` reads live Overleaf state. Its `data` contains:

```json
{
  "project": {"id": "project-id", "name": "My Paper"},
  "documents": [
    {
      "id": "doc-id",
      "path": "sections/introduction.tex",
      "remoteVersion": 42,
      "contentHash": "sha256:..."
    }
  ],
  "threads": [
    {
      "id": "thread-id",
      "state": "open",
      "messages": [],
      "anchors": [],
      "anchorState": "detached",
      "detachedReason": "range_missing"
    }
  ]
}
```

`comments.get` returns the same project model, one singular `thread`, and only
the documents referenced by that thread:

```console
jujuleaf bridge comments get THREAD_ID --protocol 1
```

Thread messages contain stable IDs, content, optional author identity, and
RFC 3339 `createdAt`/`editedAt` timestamps. Email addresses and private raw
Overleaf fields are not exposed.

### Anchors

An attached or collapsed anchor has this form:

```json
{
  "documentId": "doc-id",
  "path": "sections/introduction.tex",
  "state": "attached",
  "sourceRangeUtf16": {"from": 120, "to": 186},
  "visibleRangeUtf16": {"from": 116, "to": 182},
  "text": "In this paper, we...",
  "before": "preceding context",
  "after": "following context"
}
```

- Both ranges use half-open CodeMirror UTF-16 coordinates.
- `sourceRangeUtf16` addresses Overleaf's source/history representation.
- `visibleRangeUtf16` addresses the text visible after tracked deletions are
  hidden. Most consumers should use this range.
- `attached` has non-empty visible text.
- `collapsed` retains a known position but currently covers no visible text.
- A detached thread has an empty `anchors` array and an explanatory
  `detachedReason`.
- Multiple array entries preserve history-OT multi-range comments.

`before` and `after` are bounded context snippets, not complete documents. The
content hash covers the complete visible UTF-8 document.

## Event stream

```console
jujuleaf bridge comments watch --protocol 1
```

The default reconciliation interval is 60 seconds and can be changed with
`--reconcile-interval SECONDS` (minimum 5).

JujuLeaf connects the live event stream before reading the initial snapshot, so
events that race with the scan are buffered rather than missed. Initial output:

```text
stream.opened
comments.snapshot (reason = initial)
stream.ready
comment.event ...
```

`comments.snapshot.data.snapshot` is identical to `comments.list.data`. Treat
it as a complete replacement for previously known state. Periodic snapshots use
`reason = reconcile`.

Normalized `comment.event.data.type` values are:

```text
message.created
message.edited
message.deleted
thread.resolved
thread.reopened
thread.deleted
thread.changed
```

An event contains project/thread IDs, an optional message ID and actor ID, and
`observedAt`. It intentionally omits comment content and anchor internals. Read
`comments.get` after an event to obtain authoritative state.

When the transport closes, JujuLeaf emits `stream.reset`, retries with bounded
exponential backoff, then emits a full snapshot with `reason = reconnect` and a
new `stream.ready`. Ctrl+C produces `stream.closed` and exit status 0.

Overleaf does not provide a durable comment event queue. Consumers therefore
must not treat `comment.event` as an audit log; the snapshot and reconciliation
records are the source of truth.

## Errors and exit status

```json
{
  "protocol": {"name": "jujuleaf.bridge", "version": 1},
  "kind": "error",
  "operation": "comments.get",
  "ok": false,
  "error": {
    "code": "AUTH_REQUIRED",
    "message": "The selected profile is not authenticated",
    "retryable": false
  }
}
```

Stable version 1 codes:

```text
JUJULEAF_NOT_INITIALIZED
AUTH_REQUIRED
PROJECT_NOT_FOUND
THREAD_NOT_FOUND
CONNECTION_LOST
RATE_LIMITED
CONFLICT
UNSUPPORTED_PROTOCOL
INTERNAL_ERROR
```

Exit status is 0 for success, 1 for operational failure, and 2 for an
unsupported protocol selection. Consumers should branch on `error.code`, not
parse `message` or invent a more detailed numeric exit-code mapping.

## Compatibility

- Adding an optional field is compatible within protocol version 1.
- Renaming/removing a field, changing its type, or changing stream semantics is
  breaking and requires a new protocol version.
- JujuLeaf should retain the prior protocol version for a migration window.
- Consumers must reject an unsupported higher protocol instead of guessing.
- Existing non-bridge `--raw` output is outside this compatibility contract.
