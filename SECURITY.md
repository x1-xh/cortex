# Security policy

Security is a foundational concern in Cortex because autonomous agents execute model-generated commands across filesystems, shells, networks, Git repositories, and external tools.

---

## 1. Threat model and trust boundaries

In Cortex, model output is always treated as untrusted input. The runtime is the sole execution authority.

```text
[Model Output: Untrusted Input]
             |
             | Proposes structured action (JSON)
             v
[Runtime Schema Validation]
             |
             | Validated action payload
             v
[Capability and Permission Policy]
             |
             | Authorized capability check
             v
[Sandbox Boundary: Isolation Layer]
             |
             | Confined execution parameters
             v
[Tool Execution]
```

### Core principle

Prompt instructions and system prompts are not a security boundary. All constraints and boundaries must be enforced by the runtime.

A language model cannot grant itself permissions, expand its workspace boundary, or disable security checks by generating prompt text. The runtime treats all model responses as unverified proposals until checked against declared manifest permissions and workspace paths.

---

## 2. High-risk capabilities

Because Cortex executes actions autonomously, several capabilities present direct security risks:

- Arbitrary shell execution: Running commands that could modify system state or escape process confines.
- Filesystem writes and deletions: Directory traversal or modifying files outside the designated workspace.
- Network access: Outbound requests that could leak credentials or exfiltrate private repository data.
- Package installation: Executing setup scripts with arbitrary host execution.
- Git operations: Unauthorized commits, branch modifications, or pushing to remote repositories.
- Model Context Protocol (MCP): Connecting to external MCP servers that declare unverified tools.
- Third-party agent skills: Loading executable scripts or hooks from external sources.

All high-risk capabilities require explicit configuration, strict capability checks, and, where configured, interactive human approval.

---

## 3. Sandboxing and defense in depth

Cortex employs defense in depth through containerized and operating-system-level isolation backends.

### Docker container isolation

When running under `DockerSandbox`, commands execute inside an isolated container configured with restricted privileges:

- Dropped capabilities: Drops all Linux capabilities (`--cap-drop ALL`), preventing network administration and raw socket operations.
- No privilege escalation: Disallows setuid binaries and privilege escalation (`--security-opt no-new-privileges:true`).
- Read-only root filesystem: Mounts the container root image as read-only (`--read-only`), preventing modification to system binaries.
- Workspace mount: The host workspace is canonicalized and mounted into `/workspace`. Working directory paths are verified against traversal escapes before execution.
- Network isolation: Defaults to `Offline` (`--network none`) unless explicitly authorized for a task.
- Resource caps: Memory limits, CPU quotas, and process limits (`--pids-limit 100`) prevent fork bombs and runaway resource exhaustion.

### Host execution fallback

When Docker is unavailable, Cortex falls back to `HostSandbox`. This backend provides process cleanup and path checks, but does not provide container virtualization:

- Working directory validation: Ensures the execution path canonicalizes within the designated workspace root.
- Environment scrubbing: Strips sensitive host environment variables (API keys, SSH agent sockets, tokens) before spawning child processes.
- Process cleanup: Child processes are attached to a process group on Unix or a Job Object on Windows, ensuring cancellation signals reap descendants.

### Sandbox limitations and non-guarantees

Sandboxing in Cortex provides defense in depth, not absolute hardware containment. Operators must account for the following limitations:

1. Shared host kernel: Containers share the host Linux kernel. Vulnerabilities in the host kernel could allow a process to escape container boundaries.
2. Workspace mutability: The designated workspace directory is mounted read-write. Sandboxed processes can modify, overwrite, or delete files inside that directory.
3. Socket mount risks: The Docker daemon socket (`/var/run/docker.sock`) must never be mounted into a container. Doing so grants full root access to the host machine.
4. Host sandbox limitations: `HostSandbox` runs under the permissions of the local user. It does not provide filesystem virtualization or kernel isolation. Untrusted code execution should always use `DockerSandbox`.

---

## 4. Multi-agent and tool trust boundaries

Cortex provides in-process messaging, supervisor-worker coordination, and external tool integration through defined boundaries:

### Inter-agent communication

- Boundary containment: Delegating a task cannot grant capabilities the child worker lacks. Permissions are configured statically per agent manifest (`AgentPermissions`); task delegation never copies, inherits, or expands capabilities across hierarchy boundaries.
- Privilege escalation prevention: An agent cannot forge its sender identity in a message envelope. `AgentManager` establishes and validates sender identities exclusively through issued `AgentEndpoint` handles in trusted host code. Deserialized envelopes cannot claim unverified authority.
- Prompt injection defense: Inter-agent messages are treated as untrusted data inputs, never as runtime execution authority. Receiving a message (`AgentMessagePayload`) enqueues data in an inbox; receipt does not execute tools, bypass validation, or perform side effects. All execution actions pass through runtime capability policies.
- Trusted host authority: Access to `AgentManager` (registration, endpoint issuance, worker assignment, and run completion) remains strictly in trusted host code. Model outputs cannot access the manager or issue endpoints.
- Hierarchy integrity: Supervisor-worker relationships form a directed acyclic graph. Direct and transitive cycles (A to B to A) are rejected at assignment time, preventing delegation deadlocks.
- Envelope deserialization: Deserializing a message envelope creates data, not authority or verified identity. Envelopes carry no cryptographic authentication.
- In-process boundary scope: Inter-agent messaging is an in-process runtime coordination abstraction; it does not isolate malicious host code with direct memory access to `AgentManager`.

### External tools and MCP boundaries

- Model Context Protocol (MCP) servers run as separate external processes communicating over standard I/O or SSE.
- MCP tools must be declared and configured in `cortex.toml`. Tools from untrusted MCP servers are disabled by default.
- Tool responses received from external MCP servers are treated as untrusted data. Any instructions contained within tool outputs cannot alter agent permissions or bypass the sandbox.

---

## 5. Security testing requirements

Every component that touches I/O, process execution, or file paths must include dedicated security test coverage:

- Path traversal attempts (`../../` outside workspace bounds)
- Shell injection and argument escaping
- Network isolation enforcement
- Timeout and cancellation enforcement
- Cancellation during stalled model HTTP requests and descendant-held shell output pipes
- Capability permission bypass prevention
- Environment variable leak prevention
- Malformed tool arguments and schema fuzzing

---

## 6. Vulnerability reporting and disclosure policy

If you discover a security vulnerability in Cortex, do not open a public issue. Follow our responsible disclosure process:

### How to report

1. Report the vulnerability privately via GitHub Private Vulnerability Reporting on the repository.
2. If GitHub Private Vulnerability Reporting is unavailable, email `security@cortex-ai.org` with details.
3. Include the following information in your report:
   - Description of the vulnerability and attack vector
   - Step-by-step reproduction instructions or proof-of-concept code
   - Affected Cortex components, versions, and configurations
   - Potential impact if exploited
   - Any suggested mitigations or patches

### Response timelines

- Acknowledgment: Maintainers will acknowledge receipt within 48 hours.
- Triage and assessment: An initial severity assessment and triage decision will be shared within 7 days.
- Patch and release: Critical issues will be patched promptly. We coordinate public disclosure with a standard 90-day window or upon publication of the security advisory release.

### CVE assignment

For confirmed vulnerabilities affecting published releases, Cortex maintainers coordinate with GitHub Security Advisories to issue a CVE identifier and publish a security advisory documenting impacted versions and upgrade guidance.
