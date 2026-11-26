# Gateway on Windows (design)

`graphrt gateway` and its `local-process` provider were written against Unix:
agents listen on Unix sockets, are stopped with `SIGTERM`/`SIGKILL` to their
process group, and the short socket directory is checked with `uid`/mode
bits. This note describes the Windows counterparts. **Linux and macOS behave
exactly as before**: every change below is either Windows-only (`cfg(windows)`)
or an addition that Unix code paths do not use.

## Transport: named pipes

| | Unix (unchanged) | Windows |
|---|---|---|
| agent listens on | `--listen unix:<socket_dir>/g…-N.sock` (mode 0600) | `--listen pipe:\\.\pipe\graphrt-<random>` |
| endpoint address | `EndpointAddress::Unix(path)` | `EndpointAddress::Pipe(name)` (new variant) |
| gateway client | reqwest over the socket (control calls), lean client (run streams) | lean client for both (reqwest has no pipe transport) |
| access control | socket file mode, private directory | default pipe DACL (write: owner, SYSTEM, administrators), local clients only, first instance only, unguessable name |

Named pipes keep the properties the Unix socket gave: no port to allocate, no
network exposure, and a name the gateway chooses (never one the agent
reports). Other local users can at most open the pipe for reading (the
default DACL), which gets them no request in; every request is signed anyway.
The agent creates the pipe with `first_pipe_instance`, so a
process that created the name earlier makes the agent fail at start-up
instead of being talked to; the name carries 128 random bits so it cannot be
created in advance. TCP loopback was the alternative; it was rejected because a
released port can be bound by any local user and the gateway would then talk
to that process.

The pipe name is stored as a fourth line of the replica's `.pid` file, so a
restarted gateway still adopts the agents of the previous one (C12). Unix pid
files are unchanged.

## Process lifecycle

| | Unix (unchanged) | Windows |
|---|---|---|
| own group | `process_group(0)` | `CREATE_NEW_PROCESS_GROUP` (the console's Ctrl+C does not reach agents) |
| graceful stop | `kill -TERM -<pgid>` | signed `POST /internal/v1/stop`: the agent drains exactly as on `SIGTERM` |
| forced stop after grace | `kill -KILL -<pgid>` | `taskkill /F /T /PID <pid>` (the agent and its children) |
| HA: kill a stale holder | `kill -KILL <pid>` | `taskkill /F /PID <pid>` |
| `graphrt serve` / `gateway` stop on | `SIGTERM`, `SIGINT` | Ctrl+C, Ctrl+Break, console close, system shutdown |

`POST /internal/v1/stop` is a new endpoint of the gateway protocol, signed like
every other `/internal/v1/*` call; the agent answers `202` and stops as if it
had received `SIGTERM` (cause `gateway stop` in the drained runs' pause
reason). It exists on every platform, but only the Windows provider sends it.

## Files and directories

- `socket_dir` on Windows holds only the `.pid` files: no `sun_path` limit, so
  the short `/tmp/graphrt-gw-<hash>` fallback and its `uid`/mode check are
  Unix-only. The directory inherits the ACL of its parent (by default the
  user's profile, or whatever the operator configured).
- HA lease files use `File::try_lock` (`flock` on Unix, `LockFileEx` on
  Windows); the lock files are never read, so Windows' mandatory locking
  does not get in the way.

## Release and tests

- `release.yml` builds the `graphrt-gateway-x86_64-pc-windows-msvc` archive too.
- The cross-platform workflow runs the whole workspace test suite with every
  feature on Windows; the local-provider, gateway and drain tests that used
  Unix sockets or signals get Windows variants (pipes, `/internal/v1/stop`).
