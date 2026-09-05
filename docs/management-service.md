> Superseded: the implemented design uses an unprivileged connection relay.
> See [relay setup](relay.md). The management service below was an earlier proposal.

# pbox management service

## Agreed constraints

- The client connects to a dedicated pbox management service.
- The service runs in a container with explicitly documented permissions.
- PVE remains the authoritative registry for boxes.
- Use supported PVE API operations for infrastructure management.
- Do not use the PVE console protocol as an execution or TCP tunnel.
- Do not require workstation SSH access to a PVE host.
- Do not make the product depend on PVE running inside Docker.
- Image authors select and install their own development and LLM tools.
- Support privately published OCI images. Keep registry credentials out of guests.

## Proposed service boundary

```text
pbox CLI -- HTTPS --> pbox management service
                         |
                         +-- PVE HTTPS API --> lifecycle, configuration,
                         |                    images and snapshots
                         |
                         +-- private guest network --> guest bootstrap
                                                      and pbox-agent
```

The service needs connectivity to the PVE API and to the guest network. This is
an explicit deployment requirement. It can be met by placing the service on
that network or providing a route from its network. Moving the service alone
does not create a route.

The client needs only the management-service URL and its own authentication
credential. The PVE token, registry credentials and guest-agent trust material
belong to the service. Guest access must not expose those credentials or give
a guest access to the management API.

The initial service should use ordinary container permissions. Do not grant
host PID access, mount host filesystems, expose a Docker socket, or request
privileged mode without a specific operation that requires them. Container
network access and PVE API privileges are separate requirements.

The existing guest SSH bootstrap is a possible internal service operation once
the service can reach the private guest network. This is separate from host
SSH. Its replacement, if needed, must have a documented provisioning interface;
do not implement it by driving a PVE terminal.

## Findings from the existing code

1. `pbox-cli/src/main.rs` connects directly from the workstation to guest
   addresses. Moving orchestration into the service resolves the workstation
   network dependency only if the service can reach those addresses.
2. `pbox-cli/src/bootstrap.rs` installs the agent through guest SSH. Agent
   installation and authenticated readiness are distinct steps.
3. The generated service waits for `network-online.target`. A live Debian guest
   received a DHCP address through ifupdown while systemd-networkd-wait-online
   delayed agent startup. The agent binds a wildcard address and does not need
   this startup dependency.
4. `pbox-agent/src/main.rs` imposes a one-hour command deadline and connection
   lifetime. Interactive LLM sessions need an explicit lifetime and reconnect
   policy before this can be considered suitable for daily work.
5. Local OCI preparation does not override Dockerfile USER or ENTRYPOINT.
   Custom images can therefore run the wrong command or fail preparation.
6. Native PVE image pulls cannot consume registry credentials stored on the
   workstation. With the service boundary, image acquisition and credential
   ownership must be explicit and consistent.
7. `podman export` supplies a filesystem, not Docker runtime configuration.
   ENV, USER, WORKDIR, ENTRYPOINT and CMD need a documented mapping or explicit
   limitations. Do not silently claim full Docker image semantics.
8. `current` resolution is implemented for deletion but is not consistently
   available for shell and other operations.
9. The existing workspace tests pass, but they did not prove the requested
   end-to-end workflow. Live tests remain a release requirement.

## Implementation sequence

1. Extract reusable box operations from the CLI into an orchestration library.
2. Define an authenticated management API with box-scoped operations and
   streamed progress. Use structured arguments, not client-supplied shell
   commands for management operations.
3. Move PVE access, image acquisition, bootstrap and guest trust to the service.
4. Route shell, exec, files and forwarding through the service. Preserve terminal
   resize, cancellation, exit status, backpressure and connection cleanup.
5. Define OCI runtime semantics and private-registry authentication. Keep tool
   installation under the image author's control.
6. Package the service and document deployment on a reachable guest network.
7. Verify private-image creation, shell, exec, files, forwarding, stop/start,
   recovery, long sessions and deletion through the installed CLI.

## Investigation state

The host-SSH implementation was reverted. Its branch and the interrupted image
changes are retained locally for reference, not as the selected architecture.
The client SSH settings added during that experiment were removed.

An API-only console probe obtained a terminal ticket and a WebSocket connection
with API-token authentication. Guest command execution was not verified. That
transport is excluded by the agreed constraints and is not an implementation
path.
