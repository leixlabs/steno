# deployment

What has to exist on the two machines before `steno-monitor` can be trusted with
a night. Nothing here is built by `cargo build`; these are the files that get
copied, enabled and forgotten.

```
deployment/
  config.example.toml        annotated server config; copy to /etc/steno-monitor/config.toml
  node.example.toml          annotated node config; tools/deploy-node.sh copies it
  systemd/
    steno-monitor.service    the recorder/server (one process, house machine)
    steno-node.service       the capture node (Pi at the bedside)
    steno-monitor.sysusers   the unprivileged service account
```

## Server (the always-on machine)

1. Account first — the unit runs as `steno-monitor`, never as root:

   ```sh
   sudo cp deployment/systemd/steno-monitor.sysusers /etc/sysusers.d/steno-monitor.conf
   sudo systemd-sysusers
   ```

2. Binary and configuration:

   ```sh
   sudo install -m755 target/release/steno-monitor /usr/local/bin/
   sudo install -d /etc/steno-monitor /var/lib/steno-monitor
   sudo install -m644 deployment/config.example.toml /etc/steno-monitor/config.toml
   ```

3. Unit:

   ```sh
   sudo install -m644 deployment/systemd/steno-monitor.service /etc/systemd/system/
   sudo systemctl daemon-reload && sudo systemctl enable --now steno-monitor
   ```

The unit refuses to start unless the recordings mount is present
(`ExecStartPre=mountpoint --quiet /mnt/steno-monitor`). If your storage lives
elsewhere, change both that line and `storage.recording_dir` — a mismatch means
the service writes to the root filesystem and loses the night on reboot.

## Node (the bedside Pi)

One command, which cross-builds, copies the binary and the unit, keeps the
existing `[archive]` / `[gate]` sections of `node.toml`, then restarts:

```sh
tools/deploy-node.sh
```

`deployment/systemd/steno-node.service` is what it installs. The service account
must be in the `audio` group, or ALSA only exposes the null device.

## Notes that are not obvious

- **Streaming transcription is not part of this directory.** The sidecar
  (`steno-stt-sidecar.app`) is a macOS-only extra, installed separately with
  `PREFIX=... apple-stt-sidecar/install.sh`; a Linux server simply leaves
  `[stt] enabled = false`.
- **Renamed paths do not migrate themselves.** A machine installed before the
  `snore-*` → `steno-*` rename still has `/etc/snore-monitor`,
  `snore-monitor.service` and `/mnt/snore-monitor`. Move them once by hand.
- Background, wiring and the shutdown order are in `AGENTS.md`; the field
  checklist with the mistakes already made once is
  `docs/operations/2026-10-29-pi-deploy.md`.
