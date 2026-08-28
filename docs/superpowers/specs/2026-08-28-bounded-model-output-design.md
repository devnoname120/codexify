# Bounded Model Output

## Problem

Several Codexify tools had result-count or timeout limits without a hard bound
on model-visible output. The dedicated `grep` tool could return a small number of
multi-megabyte lines, `run_command` retained complete stdout and stderr in memory,
and `exec_command.max_output_tokens` could raise the nominal 10,000-token default
without a server-side ceiling.

The same class of bug can recur in any built-in or bridged MCP tool, because both
MCP `content` and `structuredContent` are visible to the model. A fix confined to
`grep` would therefore leave the connector without a reliable context-window
invariant.

## Goals

- Bound every textual model-visible tool result by default.
- Keep process capture memory bounded independently from model-output size.
- Let callers request a smaller command-output budget but never raise policy.
- Preserve useful prefixes and suffixes and announce every model-facing cut.
- Keep component-only `_meta` outside the model-output policy so review widgets
  can retain their private payloads.
- Preserve MCP output-schema validity.

## Non-goals

- The policy does not estimate image tokens or alter image/resource-link blocks.
- It does not make arbitrary upstream tools pageable when their API has no paging
  arguments. Oversized structured responses fail with guidance to narrow the call.
- It does not replace the existing file, entry, tree, patch, or artifact limits.
  Those remain useful semantic and memory bounds before final result processing.

## Policy

`output.maxToolOutputTokens` is a positive integer with a default of 10,000. Token
counting uses the existing Codex-compatible approximation of four UTF-16 code
units per token.

The configured value is a ceiling, not a new default requested by each caller:

```text
effective command output = min(requested or 10,000, configured ceiling)
```

This matches the important property of current upstream Codex unified exec: a
tool call may lower its output budget, but a larger request cannot override the
model/tool-output policy. The comparison was made against upstream commit
`5eea8d0dd3f6b38b0e457d266fd7c918eb189bb6`.

## Result Finalization

Every non-self-managed tool result passes through one server-side finalizer before
conversion to the MCP response.

1. Text blocks are treated as one logical text stream and truncated in the middle
   while retaining bounded head and tail content.
2. Explicit `structuredContent` is measured by streaming JSON serialization, so
   checking its size does not allocate a second complete serialized copy.
3. If explicit structured data exceeds policy, it is omitted and the result is
   converted to an MCP error with a bounded diagnostic. Error results are exempt
   from the advertised success output schema; emitting arbitrary partial JSON
   could otherwise violate required properties, array constraints, discriminated
   unions, or references.
4. For ordinary native tools whose output schema is `{content: string}`, the
   finalizer generates the mirror only after text bounding. It reduces the text
   further when JSON escaping would make that generated mirror exceed policy.
5. Component-only `_meta`, image blocks, and resource links are preserved.
6. Truncation and the best available original token estimate are recorded in the
   existing audit metadata.

`exec_command` and `write_stdin` are self-managed because their structured receipt
contains the same bounded output string. They clamp the caller's requested budget
before constructing either representation.

## Command Capture

Model-output bounds do not prevent a child process from exhausting server memory
before its result is returned, so command capture has a separate byte ceiling.

- Resident unified exec retains at most 1 MiB between yields, split equally
  between a stable head and rolling tail.
- `run_command` now drains stdout and stderr concurrently into separate 512 KiB
  head/tail buffers, retaining at most approximately 1 MiB total.
- Output beyond a capture ceiling is counted and replaced by an explicit middle
  marker.
- Timeout kills the child, settles or aborts lingering pipe drains, and returns
  bounded partial output plus the timeout diagnostic.
- The combined one-shot result is then subjected to the configured token policy.

The byte capture ceiling and token output ceiling intentionally solve different
problems: the former protects process memory, while the latter protects model
context.

## Search Bounds

`grep` keeps its search-specific semantics in addition to the universal finalizer:

- `maxResults` defaults to 500 and cannot exceed `output.maxEntries` when that
  entry limit is enabled.
- Requested context is capped at 20 lines on each side of a match.
- A rendered result line is capped at 4,096 bytes. Matching lines are windowed
  around the actual regex match, so a minified line retains the relevant text
  rather than only its unrelated beginning and end.
- Results are appended into a byte-budgeted collector and scanning stops when the
  configured model-output budget is reached.
- The final line names every active reason for truncation: match count, output
  budget, context cap, or long-line elision.

## Failure Semantics

- Text truncation is successful output with an explicit marker.
- Oversized explicit structured data is a bounded error requesting narrower
  arguments.
- A configured budget too small to serialize even an empty required text mirror
  also produces a bounded error instead of an invalid success response.
- A zero `maxToolOutputTokens` value is rejected at startup.

## Verification

Regression coverage includes:

- strict token fit and Unicode-boundary preservation;
- generated structured mirrors containing heavily escaped text;
- oversized explicit structured data and untouched component `_meta`;
- a single enormous minified grep line with the match in its middle;
- `grep.maxResults` and context clamping;
- 2 MB `run_command` stdout and partial timeout output;
- a 70,000-token unified-exec request clamped to a 20-token server policy;
- configuration parsing and zero-budget rejection.
