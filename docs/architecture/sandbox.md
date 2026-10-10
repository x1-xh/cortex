# Sandbox architecture

The sandbox forms an isolation layer between actions proposed by model agents and the host operating system. Cortex uses this layer so that untrusted commands cannot escape designated workspace paths or consume unbounded system resources.

## Architecture overview

The sandbox sits between the runtime tool dispatcher and the underlying operating system. When an agent requests a shell command or file modification, the runtime passes the request through capability checks before handing it to a sandbox backend.

```text
[Agent Loop]
     |
     v
[Tool Registry / Capability Filter]
     |
     v
[Sandbox Trait]
     |
     +--> [Docker Sandbox]  --> (OCI Container: drop capabilities, offline network, cgroups)
     |
     +--> [Host Sandbox]    --> (Process Tree: workspace bounds, env scrub, job cleanup)
```

## Execution backends

Cortex defines the `Sandbox` trait in `crates/cortex-runtime/src/sandbox/mod.rs` with two backends:

1. `HostSandbox`: Direct process execution on the host machine. Used for local CLI runs where Docker is not installed or when running trusted local tasks.
2. `DockerSandbox`: Isolated container execution using the Docker or Podman CLI. Used for running untrusted workflows, multi-agent tasks, and automated benchmarks.

Both backends return a `SandboxExecutionResult` containing process exit code, captured standard output, standard error, and execution duration in milliseconds.

## Docker container isolation

`DockerSandbox` configures OCI containers with restricted privileges using `DockerSandboxConfig`:

- Dropped capabilities: Drops all Linux capabilities (`--cap-drop ALL`) so child processes cannot perform privileged network or system administration operations.
- No privilege escalation: Disallows setuid binaries and privilege escalation (`--security-opt no-new-privileges:true`).
- Read-only root filesystem: Mounts the container root image as read-only (`--read-only`), preventing permanent filesystem changes outside mounted directories.
- Workspace volume bind mount: The host workspace is canonicalized and mounted into `/workspace`. Before execution, relative working directory paths are verified to ensure they resolve strictly within the workspace root. Traversal escapes attempting to leave the workspace are rejected with `PermissionDenied`.
- Network isolation policies: Supports three network modes via `NetworkIsolationPolicy`:
  - `Offline` (`--network none`): Complete network egress block. This is the default setting.
  - `IntranetOnly` (`--network internal`): Traffic restricted to user-defined container networks without internet routing.
  - `FullInternet` (`--network bridge`): Standard container network bridge.
- Resource controls:
  - Memory caps: Configured through `--memory <bytes>` (default 512 MB).
  - CPU quotas: Configured through `--cpus <quota>` (default 1.0 CPU).
  - Process limits: Configured through `--pids-limit <count>` (default 100) to block fork bombs.
- Execution timeouts: Commands run under a deadline (default 30 seconds). Processes exceeding the timeout are terminated.
- Ephemeral lifecycle: Containers run with `--rm` so state is discarded upon command completion.

## Host execution boundary

When Docker is unavailable, `HostSandbox` executes commands directly on the host operating system with minimal protections:

- Working directory validation: Ensures the command execution directory canonicalizes inside the configured workspace root.
- Environment scrubbing: Strips sensitive host environment variables (such as cloud provider credentials, SSH agent sockets, and API keys) before spawning the child process.
- Process tree cleanup: Child processes are attached to a dedicated process group on Unix or a Job Object on Windows, enabling cancellation routines to terminate child processes when cancelled or timed out.

## Capability permissions enforcement

Agent manifests declare static capabilities in their manifest YAML:

```yaml
permissions:
  filesystem: workspace
  shell: true
  network: false
  git:
    read: true
    write: true
    push: false
```

The runtime enforces these capabilities before invoking sandbox backends:

- `filesystem`: Restricts file modifications strictly to the workspace root. Absolute paths outside the workspace or directory traversals (`../`) fail validation.
- `shell`: If set to `false`, all command execution requests are blocked by the tool registry.
- `network`: If set to `false`, `DockerSandbox` forces the `Offline` network policy.
- `git`: Controls which Git subcommands the agent can invoke. Disallows destructive commands like `push` or rebase unless explicitly enabled.

## Sandbox limitations and non-guarantees

Sandboxing in Cortex provides defense in depth. It does not provide absolute hardware isolation. Operators must account for the following technical limitations:

1. Shared host kernel: Docker containers share the host Linux kernel. Any unpatched kernel vulnerability or privilege escalation in the host kernel could allow a process to break container boundaries.
2. Volume mount risks: Because the workspace directory is mounted read-write into the container, a sandboxed process can modify, corrupt, or delete files inside that specific workspace directory. Host files outside the workspace remain protected, but workspace contents are mutable by design.
3. Symlink traversal across volumes: If a tool creates a symlink inside the workspace that points to a target outside the workspace, subsequent operations must resolve links carefully. The canonicalization checks in `DockerSandbox::build_docker_args` verify targets on the host before constructing container paths.
4. Host socket exposure: The Docker daemon socket (`/var/run/docker.sock`) must never be mounted into a sandbox. Doing so grants full root access to the host machine.
5. HostSandbox has no filesystem virtualization: `HostSandbox` is not a container. While it verifies working directories and scrubs environment variables, any process running under `HostSandbox` shares the host filesystem permissions of the running user. For untrusted code execution, `DockerSandbox` must be used.
