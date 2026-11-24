# Kubernetes concepts in devsandbox

What a Kubernetes concept corresponds to in devsandbox, for readers who
know one and meet the other. Each row is the closest thing the code has;
"none" means nothing here plays that part (with the plan, where one
exists). The analogies are loose: devsandbox has no central store and
no desired-state API; each host's daemon acts on its own files and its
runtime.

## Cluster

| Kubernetes | devsandbox | Where |
|---|---|---|
| Cluster | the fleet: every machine is a member ([ADR 0043](../decisions/0043-every-machine-is-a-fleet-member.md)) | [fleet.md](../architecture/daemon/fleet.md) |
| Node | a machine, a fleet member: `machines.toml` (the machines this host links to), `member.toml` (this host's own name, `control_plane`, `trust`, `[allow]`, `[caps]`) | `serve::machines` (`Entry`), `serve::fleet::member` |
| Node status, allocatable | the capacity report each daemon's snapshots carry; `member.toml`'s `[caps]` are soft caps | `serve::capacity`; [Capacity](../architecture/daemon/fleet.md#capacity) |
| kubelet | the per-host daemon `devsandbox serve`: runs the verbs, polls the runtime, holds the bridges to every instance's helper | `src/serve/`; [role.md](../architecture/daemon/role.md) |
| kube-apiserver | each daemon's API (one unix socket per user and host), and the control plane's federated view of every member's instances | `serve::api`, `serve::snapshots::merge`; [api.md](api.md) |
| kube-scheduler | placement on the control plane: scores members' capacity, image and repo locality; `run --place` from a member asks it with `fleet.place` | `serve::fleet::place::place`; [Placement](../architecture/daemon/fleet.md#placement) |
| etcd | files are the truth on each host ([ADR 0001](../decisions/0001-files-are-the-truth-the-daemon-is-replaceable.md)): `state.toml`, the membership log `membership.json` (replicated by `fleet.sync`), authority-signed config versions under `mesh/tree/` ([ADR 0049](../decisions/0049-the-authority-signs-a-config-every-member-runs.md)), the control plane's saved rows `remotes.json` | `src/state.rs`, `serve::fleet::membership`, `serve::fleet::mesh`, `serve::machines::saved` |
| Cluster CA, kubeadm PKI | the fleet's authority signs the membership log; each member has an ed25519 key (`member.key`) every peer link proves | `serve::fleet::identity` (`FileKeyStore`), `serve::fleet::membership`, `serve::fleet::proof`; [Member identity](../architecture/daemon/fleet.md#member-identity) |
| `kubeadm join`, TLS bootstrapping, CSR approval | `machine add` (the target's key fingerprint confirmed, the authority signs its `add`, another member signs a capped introduction); `machine pending\|approve\|deny` on the authority | `serve::machines::join`, `serve::fleet::proposals`, `serve::machines::changes`; [Approvals and pushes](../architecture/daemon/fleet.md#approvals-and-pushes) |
| Node authorizer, RBAC | the trust table for every remote caller (`trust = "worker"\|"full"`), the worker policy over placed work's run and build argvs with `member.toml`'s `[allow]`, and exposure over the link graph | `serve::daemon::Table`, `serve::fleet::lent`, `worker_policy::Policy`, `serve::fleet::graph::exposure`; [Trust](../architecture/daemon/fleet.md#trust) |
| Konnectivity (agent-initiated tunnels) | peer links a member dials (`fleet-link --peer` over ssh) and routed streams through other members to one with no link here | `serve::fleet::dialer`, `serve::fleet::route`, `serve::fleet::routes`; [Peer links](../architecture/daemon/fleet.md#peer-links), [Routed streams](../architecture/daemon/fleet.md#routed-streams) |
| Cluster upgrade, version skew policy | members follow the build the authority pins in the membership log | `serve::fleet::update`; [Versions](../architecture/daemon/fleet.md#versions) |
| controller-manager | reconcilers in the daemon: the once-per-boot autostart pass, the forwards registry over `forwardPorts`, bridges ensured per running instance, dispatchers driving child instances | `commands::autostart`, `serve::forwards::Registry`, `commands::dispatch`; [autostart.md](../architecture/automations/autostart.md), [dispatchers.md](../architecture/automations/dispatchers.md) |
| k3s single binary | one `devsandbox` binary, the static in-container helper `devsbd` embedded in it | `build.rs`, `src/devsbd.rs` (`blob`, `install`); [helper overview](../architecture/helper/overview.md) |

## Workloads

| Kubernetes | devsandbox | Where |
|---|---|---|
| Pod | an instance: one container `devsandbox-<instance>`, its `isolated` services on a per-instance network (loosely sidecars: separate containers, not one network namespace) | `commands::run`, `commands::services`; [services.md](../architecture/instances/services.md) |
| PodSpec, Deployment template | `[sandbox.*]` and `[template.*]`, `extends` deep-merging (arrays concatenate) | `src/config.rs`; [config.md](../architecture/config.md) |
| Namespace | loosely a config root; a peer's lent work lands in its own fleet root `fleet/<peer>/` | `worker_policy::fleet_root_of` |
| Container image | the base image or build plus a derived image with devcontainer features | `commands::run::image`; [images-and-features.md](../architecture/instances/images-and-features.md) |
| Labels | container labels: `config_hash` and `build_hash` (drift), `devsandbox.sandbox` | `commands::drift_decision`; [rebuild-and-drift.md](../architecture/instances/rebuild-and-drift.md) |
| `restartPolicy` | `autostart = "runtime"` (the runtime's restart policy where the backend has one) or `true` (the daemon's once-per-boot pass) | `commands::autostart`, `Backend::set_restart_policy` |
| CRI, containerd | the `Backend` trait over docker, podman and Apple `container`; differences are capabilities ([ADR 0012](../decisions/0012-runtime-differences-are-backend-capabilities.md)) | `runtime::Backend`; [runtimes.md](../architecture/runtimes.md) |
| RuntimeClass | none ([isolation](../plans/active/isolation.md)) | |
| Device plugins, extended resources | GPUs only: `hostRequirements.gpu`, a worker's capacity report says whether it lends one, placement filters on it | `runtime::gpu`, `place::gpu_check`; [ADR 0056](../decisions/0056-a-gpu-is-placed-where-a-worker-lends-one.md) |
| CNI | per-instance (or per-root, `global`) networks on one host | `commands::services`, `commands::subnet`; [Networks](../architecture/instances/services.md#networks) |
| Service, kube-proxy | none across machines ([mesh services](../plans/active/mesh-services.md)); on one host a service is reached by its network alias | [Reaching a service](../architecture/instances/services.md#reaching-a-service) |
| `kubectl port-forward` | the daemon's forwards (ad-hoc and `forwardPorts`) and `devsandbox port`, tunnelled through the helper, over `TUNNEL` streams to another machine | `serve::forwards`, `commands::port`, `commands::tunnel`; [forwards.md](../architecture/daemon/forwards.md) |
| `kubectl exec`, `attach` | `devsandbox exec`: a tty exec is a helper session that outlives its client; `exec --attach` resumes one | `commands::exec`, `devsbd/src/sessions.rs`; [Sessions](../architecture/helper/in-container.md#sessions-devsbd-session-newattachlsrmprune) |
| `kubectl logs` | `devsandbox logs` | `commands::logs` |
| `kubectl top` | `devsandbox ps` (CPU, MEM) | `commands::ps` |

## No Kubernetes equivalent

- **Worktrees**: repeat instances of one repo each get a git worktree
  ([worktrees.md](../architecture/instances/worktrees.md)).
- **Moving code and `fetch`**: a placed run's config root and repos go to
  the worker, its branch and work in progress come back as hidden refs
  ([Moving code](../architecture/daemon/fleet.md#moving-code),
  `commands::fetch`).
- **Devcontainer features** baked into a derived image
  ([images-and-features.md](../architecture/instances/images-and-features.md)).
- **Opening URLs**: a container's `xdg-open` opens on the member the user
  last attached from
  ([Opening URLs](../architecture/daemon/fleet.md#opening-urls)).
- **ssh-agent relay**: the host's agent reaches the container through the
  helper ([ssh-agent.md](../architecture/helper/ssh-agent.md)).

## Transport security

Kubernetes encrypts control-plane traffic with TLS; pod-to-pod traffic is
plaintext unless the CNI or a service mesh adds encryption. A devsandbox
link rides its transport's security (ssh: encryption and the host key,
[Transport](../architecture/daemon/fleet.md#transport)), and a routed
stream is sealed end to end between origin and destination, so relays
see only ciphertext
([ADR 0046](../decisions/0046-routed-streams-are-end-to-end-sessions.md)).
