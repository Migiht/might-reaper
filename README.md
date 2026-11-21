# might-reaper

A tiny daemon that stops idle memory hogs on a desktop:

- **Claude Desktop session workers.** Claude Desktop keeps one `claude` CLI process alive for every
  session it has opened, until the session is archived or the app quits. Each one is a Bun binary
  costing 100–400 MB of private memory. Stopping one loses nothing: the transcript is on disk, and
  the app starts a new worker with `--resume` as soon as the session gets its next message.
- **Gradle daemons and the Kotlin compile daemon.** Each is allowed `-Xmx4g` and stays up for hours
  after a build, mostly in swap. The next build starts new ones; it is only slower to warm up.
- **Plasma Discover**, which holds a gigabyte or more long after it was last looked at.

`plasmashell` and other Plasma processes are never touched.

## Install

```sh
cargo build --release && install -m755 target/release/might-reaper ~/.local/bin/
```

`~/.config/systemd/user/might-reaper.service`:

```ini
[Unit]
Description=Stop idle Claude workers, Gradle/Kotlin daemons and Plasma Discover

[Service]
ExecStart=%h/.local/bin/might-reaper
Restart=on-failure
Nice=10
MemoryMax=16M

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now might-reaper
journalctl --user -u might-reaper -f
```

## Use

```
might-reaper [--interval DUR] [--idle DUR] [--daemon-idle DUR] [--sample DUR] [--cpu PCT]
             [--grace DUR] [--only SUBSTR] [--dry-run] [--list]
```

| Option | Default | Meaning |
|---|---|---|
| `--interval` | `1h` | How often idle Claude workers are stopped |
| `--idle` | the interval | How long a Claude worker must have been idle |
| `--daemon-idle` | `15m` | How long a Gradle/Kotlin daemon or Plasma Discover must have been idle (checked every sample) |
| `--sample` | `5m` | How often activity is sampled |
| `--cpu` | `1` | CPU percent (of one core) between samples that counts as activity |
| `--grace` | `120s` | Children started this soon after a Claude worker are its MCP servers |
| `--only` | | Only stop processes whose name (Claude session name, or `Gradle daemon`, `Kotlin compile daemon`, `Plasma Discover`) contains this text |
| `--dry-run` | | Log what would be stopped, stop nothing |
| `--list` | | Print targets with private memory, CPU over 3 s and late children |

Durations: `90`, `30s`, `5m`, `1h`, `1d`.

## How it works

- **Targets**
  - processes named `claude` started with `--output-format stream-json --input-format …` — what
    Claude Desktop runs. Interactive terminal sessions are never touched;
  - `java` processes whose main class is `org.gradle.launcher.daemon.bootstrap.GradleDaemon` or
    `org.jetbrains.kotlin.daemon.KotlinCompileDaemon`;
  - the process named `plasma-discover`.
- **Activity** is a sample in which the process used more than `--cpu` percent of a core. A Claude
  worker is also active while it has a child started more than `--grace` after itself (a Bash tool
  call, a background task).
- Every `--interval`, Claude workers with no activity for `--idle` get `SIGTERM`. Daemons and Discover
  are checked at every sample and get `SIGTERM` after `--daemon-idle` without activity.
- Newly seen processes count as active, so a restarted daemon waits a full idle period before
  stopping anything. Processes are keyed by pid and start time, and the identity is checked again
  right before the signal.
- A Claude session waiting on a permission prompt for longer than `--idle` is stopped too; the prompt
  is lost, the conversation is not.
