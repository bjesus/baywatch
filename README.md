# baywatch

Run all your web apps locally, always accessible, without wasting resources. Baywatch is a reverse proxy that starts services on demand and stops them when idle.

Point your browser at `myapp.localhost` and baywatch spawns the configured command, waits until it's ready, and proxies the request through. After a period of inactivity, the service is shut down. The next request starts it again.

It's completely agnostic about how your services run - use Docker, docker-compose, npm, npx, uvx, a plain binary, or anything else that listens on a port.

## Configuration

Baywatch looks for its config at `~/.config/baywatch/config.yaml`, falling back to `/etc/baywatch/config.yaml`. You can override this with `--config <path>`.

```yaml
bind: 0.0.0.0
port: 80
domain: localhost

services:
  myapp:
    port: 3000
    command: npm start
    pwd: ~/projects/myapp
    sleep_after: 300

  api:
    port: 8000
    command: cargo run --release
    pwd: ~/projects/api

  freetar:
    port: 22001
    command: uvx freetar
    sleep_after: 60
    env:
      FREETAR_PORT: "22001"
```

Each service is accessible at `<name>.<domain>` - with the config above, `http://myapp.localhost` proxies to port 3000.

### Service options

| Field | Required | Default | Description |
|---|---|---|---|
| `port` | yes | | Port the service listens on |
| `command` | yes | | Shell command to start the service |
| `pwd` | no | cwd | Working directory (supports `~`) |
| `sleep_after` | no | 600 | Seconds of inactivity before shutdown |
| `env` | no | | Environment variables for the command |

### Global options

| Field | Default | Description |
|---|---|---|
| `bind` | 0.0.0.0 | Address to listen on |
| `port` | 80 | Port to listen on |
| `domain` | localhost | Base domain for routing |
| `shutdown_grace` | 5 | Seconds between SIGTERM and SIGKILL |
| `startup_timeout` | 30 | Seconds to wait for a service to respond |

## Running

Start directly:

```
baywatch
```

Or as a systemd user service:

```
systemctl --user enable --now baywatch
```

Set `RUST_LOG=debug` for verbose output.

## Installing

### Arch Linux (AUR)

```
# Using an AUR helper
paru -S baywatch
```

### Debian/Ubuntu

Download the `.deb` from [Releases](https://github.com/bjesus/baywatch/releases):

```
sudo dpkg -i baywatch_0.1.0_amd64.deb
```

### NixOS

Add the flake to your inputs and use the provided NixOS module or package.

### Static binary

Download a static musl binary from [Releases](https://github.com/bjesus/baywatch/releases) and put it in your `PATH`. Run `sudo setcap cap_net_bind_service=+ep /usr/local/bin/baywatch` to allow binding port 80 without root.

### From source

```
cargo install baywatch
```
