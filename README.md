<h1 align="center">
Baywatch
</h1>
<p align="center">
<img width="300" alt="baywatch" src="https://github.com/user-attachments/assets/3daf2140-1ffa-41c1-8bb4-7a9d35e3083e" />
</p>

Baywatch runs webapps on-demand, and reverse-proxy them so they're available at `http://example.localhost/`.

No need to run `npm run dev`, `docker-compose up` etc. Your apps will spin up when you make an HTTP request, and shut down after your configured inactivity period.
Configure as many apps as you want - they take zero resources unless you use them, and you'll never need to remember a port number again.

## Configuration

Baywatch looks for its config at `~/.config/baywatch/config.yaml`, falling back to `/etc/baywatch/config.yaml`. You can override this with `--config <path>`.

```yaml
bind: 0.0.0.0
port: 80
domain: localhost
idle_timeout: 900

services:
  myapp:
    port: 3000
    command: npm start
    pwd: ~/projects/myapp
    idle_timeout: 300

  api:
    port: 8000
    command: cargo run --release
    pwd: ~/projects/api

  freetar:
    port: 22001
    command: uvx freetar
    idle_timeout: 60
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
| `idle_timeout` | no | 600 | Seconds of inactivity before shutdown (overrides global) |
| `env` | no | | Environment variables for the command |

### Global options

| Field | Default | Description |
|---|---|---|
| `bind` | 0.0.0.0 | Address to listen on |
| `port` | 80 | Port to listen on |
| `domain` | localhost | Base domain for routing |
| `idle_timeout` | 600 | Default seconds of inactivity before shutdown |
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
yay -S baywatch
```

### Debian/Ubuntu

Download the `.deb` from [Releases](https://github.com/bjesus/baywatch/releases):

```
sudo dpkg -i baywatch_0.1.1_amd64.deb
```

### NixOS

Add the flake to your inputs and use the provided NixOS module or package.

### Static binary

Download a static musl binary from [Releases](https://github.com/bjesus/baywatch/releases) and put it in your `PATH`. Run `sudo setcap cap_net_bind_service=+ep /usr/local/bin/baywatch` to allow binding port 80 without root.

### From source

```
cargo install baywatch
```
