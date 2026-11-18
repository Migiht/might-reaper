# claude-reaper

A tiny daemon that stops idle Claude Desktop session workers.

Claude Desktop keeps one `claude` CLI process alive for every session it has opened, until the
session is archived or the app quits. Each one is a Bun binary costing 100–400 MB of private memory
(plus ~50–100 MB of the binary's pages, shared between them). Ten sessions opened over a day keep
1.5–2 GB resident, though only one or two are in use.

Stopping a worker loses nothing: the transcript is on disk, and the app starts a new worker with
`--resume` as soon as the session gets its next message.

Runtime tuning does not help here. On Linux, `BUN_OPTIONS=--smol`, `MIMALLOC_PURGE_DELAY=0`,
`BUN_GC_TIMER_INTERVAL`, `BUN_IDLE_GC_SECONDS`, `BUN_GARBAGE_COLLECTOR_LEVEL` and
`BUN_JSC_useFTLJIT=0` all leave an idle worker at ~110 MiB private; only `BUN_JSC_useJIT=0` saves
~15 MiB, at a large CPU cost.

## Install

```sh
cargo install --git https://github.com/Migiht/claude-reaper
```

`~/.config/systemd/user/claude-reaper.service`:

```ini
[Unit]
Description=Stop idle Claude Desktop session workers

[Service]
ExecStart=%h/.local/bin/claude-reaper
Restart=on-failure
Nice=10
MemoryMax=16M

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now claude-reaper
journalctl --user -u claude-reaper -f
```

## Use

```
claude-reaper [--interval DUR] [--idle DUR] [--sample DUR] [--cpu PCT] [--grace DUR]
              [--only SUBSTR] [--dry-run] [--list]
```

| Option | Default | Meaning |
|---|---|---|
| `--interval` | `1h` | How often idle workers are stopped |
| `--idle` | the interval | How long a worker must have been idle |
| `--sample` | `5m` | How often activity is sampled |
| `--cpu` | `1` | CPU percent (of one core) between samples that counts as activity |
| `--grace` | `120s` | Children started this soon after the worker are its MCP servers |
| `--only` | | Only stop sessions whose name contains this text |
| `--dry-run` | | Log what would be stopped, stop nothing |
| `--list` | | Print workers with private memory, CPU over 3 s and late children |

Durations: `90`, `30s`, `5m`, `1h`, `1d`.

## How it works

- **Workers** are processes named `claude` started with `--output-format stream-json
  --input-format …` — what Claude Desktop runs. Interactive terminal sessions are never touched.
- **Activity** is a sample in which the worker used more than `--cpu` percent of a core (an idle one
  sits at ~0.4% for Bun's GC and timers), or in which it has a child started more than `--grace`
  after itself (a Bash tool call, a background task).
- Every `--interval`, workers with no activity for `--idle` get `SIGTERM`. Newly seen workers count as
  active, so a restarted daemon waits a full idle period before stopping anything. Workers are keyed
  by pid and start time, and the identity is checked again right before the signal.
- A session waiting on a permission prompt for longer than `--idle` is stopped too; the prompt is
  lost, the conversation is not.
