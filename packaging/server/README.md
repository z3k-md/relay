# Relay home server

An always-on Relay device with no UI. It joins every space a paired device
shares with it and keeps each folder under one data directory
(`<data>/<space>/<mount>`), so your other devices can catch up from it when
they are never online together. Plan and later stages:
[`docs/proposals/home-server.md`](../../docs/proposals/home-server.md).

It is an ordinary peer. It does not decide conflicts, and your devices keep
syncing with each other when it is off.

## Linux (systemd)

```sh
./scripts/install.sh                      # builds ~/.cargo/bin/relay
sudo install -m 0755 ~/.cargo/bin/relay /usr/local/bin/relay
sudo useradd --system --home-dir /var/lib/relay --create-home relay
sudo -u relay env RELAY_HOME=/var/lib/relay/home relay init --name nas
sudo -u relay env RELAY_HOME=/var/lib/relay/home relay server enable --data /var/lib/relay/data
sudo install -m 0644 packaging/server/relay-server.service /etc/systemd/system/
sudo systemctl enable --now relay-server
```

Open UDP 47321 in the firewall for devices on your LAN.

## Docker

```sh
docker compose -f packaging/server/compose.yaml up -d --build
```

The device and its data live in the `relay` volume. The server answers on
UDP 47322 of the host, so a Relay app on the same machine keeps 47321.
LAN discovery does not reach into a container, so pair with an address
(below).

## Pair and share

On the server, start pairing and allow your computer to manage it:

```sh
# systemd
sudo -u relay env RELAY_HOME=/var/lib/relay/home relay pair --allow-manage
# Docker
docker exec -it relay-server relay pair --allow-manage
```

Enter the code on your computer (Peers, then Pair a device). Add the
server's address when it is not found on the LAN, for example
`192.168.1.20:47322` for the Docker setup.

Then share a space with the server from your computer (or
`relay share SPACE SERVER`). The server joins it and attaches every folder
within a few seconds. `relay server status` on the server lists what it
keeps.

Turning the role off (`relay server disable`) stops joining new spaces.
Spaces, folders, and files stay.

## Windows desktop as a stand-in

Until there is real hardware, the Docker setup runs on Windows with Docker
Desktop. Docker Desktop must be running for the server to be up. Allow
inbound UDP 47322 in Windows Defender Firewall so the Mac can reach it, and
pair the Mac with the PC's LAN address and port 47322.

Running the Linux binary under WSL2 also works for testing (`relay run`
in a terminal). WSL's default NAT networking hides it from the LAN; use
mirrored networking (`networkingMode=mirrored` in `.wslconfig`) and a port
other than the Windows app's 47321 (`relay run --listen 0.0.0.0:47322`).
