# [Codexify](https://codexify.dev/)

Landing page: **https://codexify.dev/**

---------------

**Turn ChatGPT into a Codex-like coding environment — without using your Codex/Work quota.**

Codexify gives a regular ChatGPT conversation the tools to work on your projects:
file editing, terminal commands, Git, interactive diffs, and reusable skills.
Describe a task, let ChatGPT inspect the code, make changes and run tests, then
review the result — all from the same conversation.

**ChatGPT does the reasoning; your machine runs the tools.** Codexify connects
them through [Model Context Protocol (MCP)](https://modelcontextprotocol.io/),
rather than running a Codex agent or starting a ChatGPT Work task. The normal
workflow uses your ChatGPT allowance, leaving your Codex/Work quota untouched.

> ChatGPT's [own usage limits](https://help.openai.com/en/articles/20001354-gpt-56-in-chatgpt)
> still apply. If you explicitly invoke Codex, Work, or another metered service
> through connected tools, that service's usage counts normally.

[Get started](#get-started) · [Documentation](#documentation) · [Website](https://codexify.dev) · [Downloads](https://github.com/devnoname120/codexify/releases/latest)

## ChatGPT plan comparison

Codexify runs inside an ordinary ChatGPT **Chat**, so the relevant limits are the Chat allowances rather than the Work/Codex quota. Codexify doesn’t use any Codex/Work quota, it only uses Chat quota which is independent:

| Plan | 5.6&nbsp;Luna | 5.6&nbsp;Sol | 5.6&nbsp;Sol&nbsp;Pro | 6&nbsp;Astra&nbsp;Pro | Context<br>window |
| :--- | :---: | :---: | :---: | :---: | ---: |
| **Free** | ∞ | — | — | — | Varies |
| **Go**<br><sub>$8/month</sub> | ∞ | — | — | — | 256K |
| **Plus**<br><sub>$20/month</sub> | ∞ | ∞<br><sub>High</sub> | — | — | 256K |
| **Pro 5x**<br><sub>$100/month</sub> | ∞ | ∞<br><sub>Extra High</sub> | **50/week**<sup>1</sup> | **50/week**<sup>1</sup> | 400K |
| **Pro 20x**<br><sub>$200/month</sub> | ∞ | ∞<br><sub>Extra High</sub> | **1,190/week**<br><sub>170/day max</sub> | **200/week** | 400K |

<sup>1</sup> One shared 50-message weekly allowance across 5.6 Sol Pro and 6 Astra Pro.

Note: the context window is the total shared reasoning window, not the amount reserved exclusively for user input: system instructions, tools, memory, internal reasoning, and the
response also consume it.

See OpenAI's [Plan tiers details](https://chatgpt.com/pricing/#:~:text=Compare%20features%20across%20plans) for more information.

## A Codex-like workflow in ChatGPT

- **Explore, edit, and test.** Search a codebase, apply patches, run your project's
  tests, and inspect the results.
- **Review the work.** See file changes in an interactive diff card, and exchange
  attachments and generated files with ChatGPT.
- **Keep projects organized.** Connect one repository or choose a project for each
  conversation. Optional Git worktrees keep concurrent edits in separate checkouts.
- **Bring your conventions.** Load `AGENTS.md` and skills, and save task notes and
  plans for later conversations.
- **Connect more tools.** Make local or remote MCP servers available through the
  same connector, including servers configured in Codex.

## Get started

Codexify runs on **macOS, Linux, and Windows**. You need a ChatGPT account or
workspace with [custom MCP apps enabled](https://developers.openai.com/api/docs/guides/developer-mode)
and access to [OpenAI Secure MCP Tunnels](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)
in an OpenAI Platform organization. ChatGPT access and tunnel permissions are
separate; a workspace administrator may need to grant them.

### 1. Install

**macOS / Linux**

```sh
curl -q -fsSL https://codexify.dev/install.sh | sh
```

**Windows PowerShell**

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://codexify.dev/install.ps1 | iex"
```

The installer verifies the downloaded binary's checksum and adds it to your PATH.
When a config already exists it also refreshes the per-user background service;
on a first install, quickstart creates the config and installs the service. No
Rust toolchain is needed. Open a new terminal after installation so the
`codexify` command is available.

macOS release binaries use the stable Developer ID identity `dev.codexify` and
are published only after Apple accepts both Intel and Apple-silicon
notarizations. The first signed update from an older ad-hoc build may require one
final macOS privacy reapproval; later signed updates retain the same identity.

Prefer a manual install? Get a [prebuilt binary](https://github.com/devnoname120/codexify/releases/latest)
or read the [installation guide](https://github.com/devnoname120/codexify/wiki/Installation).
The installer source is available for [macOS/Linux](https://github.com/devnoname120/codexify/blob/main/install.sh) and [Windows](https://github.com/devnoname120/codexify/blob/main/install.ps1).

### 2. Connect to ChatGPT

```sh
codexify quickstart
```

The wizard defaults to multi-project mode, asks for the matching projects root or
single project directory, then guides you through creating a tunnel, its runtime
API key, and the `Codexify` connector in ChatGPT. It prints the links and exact
settings to use, saves your configuration, and starts the connection.
Keep Codexify running while you use it; the background service handles that for a
standard installation.

This setup uses an outbound OpenAI tunnel: you do not need a public server URL or
an inbound port. For other deployments, see [connection options](docs/REFERENCE.md#connecting-to-chatgpt).

### 3. Start a task

Start a regular **Chat** conversation in ChatGPT, not a Work or Codex task, and
select the connector you created. If you enabled multiple projects, give ChatGPT
the project path or choose one when asked. Start with a read-only request:

```text
Call get_agent_brief and follow the project's instructions.
Explain how this project is organized. Do not change any files.
```

Then ask for a concrete change, for example:

> Fix the failing test, run it again, and show me the diff. Do not commit yet.

A conversation stays attached to its selected project. Start another chat for a
different project; see the [workspace guide](https://github.com/devnoname120/codexify/wiki/Multi-Project-Mode)
for project selection and worktree options.

## Use it safely

**Codexify runs real commands with your user account's permissions.** Shell
execution is unrestricted by default. Project boundaries and Git worktrees are
not an operating-system sandbox, and commands can reach beyond the project.

Start with a trusted repository, review requested actions, and keep credentials
out of chats and version control. A private tunnel keeps the MCP endpoint off the
public internet; it does not make inference local. Code and tool results returned
to ChatGPT are sent to OpenAI.

See the [security reference](docs/REFERENCE.md#security) for access controls,
command restrictions, and the authority of connected MCP servers.

## Help and everyday use

| Task | Command |
| --- | --- |
| Check configuration and connectivity | `codexify doctor` |
| Print or inspect configuration | `codexify config` · `codexify config get <key>` |
| Edit configuration | `codexify config edit` |
| Check the background service state | `codexify service status` |
| Follow the formatted service log | `codexify service logs -f` |
| Stop the background service | `codexify service stop` |
| Start it again | `codexify service start` |
| Restart it without changing login startup | `codexify service restart` |

To update a standard installation, ask ChatGPT to update Codexify. After the
service restarts, open the connector in ChatGPT Settings and click **Refresh** at
the bottom of its tool list. The [update guide](docs/REFERENCE.md#self-update)
explains the process.

For setup problems, start with [Troubleshooting](https://github.com/devnoname120/codexify/wiki/Troubleshooting).
Report reproducible bugs in [GitHub Issues](https://github.com/devnoname120/codexify/issues).

## Documentation

| Looking for... | Read |
| --- | --- |
| Setup and usage guides | [Wiki](https://github.com/devnoname120/codexify/wiki) |
| CLI options, configuration, and tool behavior | [Technical reference](docs/REFERENCE.md) |
| Additional MCP servers and reusable workflows | [MCP bridging](docs/REFERENCE.md#bridging-other-mcp-servers) · [Skills](docs/REFERENCE.md#skills) |
| Building or contributing | [Architecture](docs/ARCHITECTURE.md) · [Development commands](docs/REFERENCE.md#dev-commands) |

## License

[MIT](LICENSE). Codexify is an independent project, not affiliated with OpenAI.

## Related projects

### ChatGPT connectors and local MCP servers

These connect a ChatGPT conversation to tools on your machine, as Codexify does.

| Project | Focus |
| --- | --- |
| [CodexPro](https://github.com/rebel0789/codexpro) | Local coding tools, attachment import, and handoff plans. |
| [local-dev-mcp](https://github.com/harukary/local-dev-mcp) | Registered projects with file, Git, shell, browser, and mobile tools. |
| [codex-free](https://github.com/hypnguyen1209/codex-free) | Rust MCP bridge with coding tools, saved context, skills, and MCP aggregation. |
| [chatgpt-web-oauth-mcp](https://github.com/escapeWu/chatgpt-web-oauth-mcp) | OAuth connector with background jobs, terminal sessions, and optional CLI-agent delegation. |
| [Agentic MCP](https://github.com/hugolsramos01-bit/mcp-agentic-server) | Structured editing, Git worktrees, semantic navigation, and checkpoints. |
| [Local Coding Agent](https://github.com/LongNgn204/local-coding-agent) | MCP workspace with desktop tray apps, a dashboard, and browser-preview tools. |

### Browser harnesses and Codex model providers

These bring web-chat models into a coding client rather than making ChatGPT the
primary interface.

| Project | Focus |
| --- | --- |
| [codex-chatgpt-web](https://github.com/miuuyy/codex-chatgpt-web) | ChatGPT Web as selectable Codex models through an embedded browser, with optional access to the active Codex task's tools. |
