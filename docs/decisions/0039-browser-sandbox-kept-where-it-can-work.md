# 0039: The browser sandbox is kept where it can work; the off-knobs are a fallback

Status: accepted.

## Context

Chromium's sandbox (and Electron's) needs a non-root euid and unprivileged
user namespaces. Docker's default seccomp profile blocks both
`CLONE_NEWUSER` and the zygote's `clone(CLONE_NEWPID)`, and on root plans
every exec is euid 0. The easy answer, `--no-sandbox` everywhere, removes
the barrier between a renderer exploit and the workspace, the ssh-agent
socket and the docker socket, which matters most for agents browsing
untrusted pages.

## Decision

- On docker, sandboxes are created with devsandbox's seccomp profile:
  moby's default, vendored verbatim and pinned by version, plus rules for
  `clone`/`unshare` with `CLONE_NEWUSER` and `clone` with `CLONE_NEWPID`
  without `CAP_SYS_ADMIN`. `userns = false` opts out. AppArmor stays on.
- Whether the sandbox works is checked in the real container after create
  (`devsbd userns check`, as the exec user), not assumed and not in the
  canary. The verdict and the env derived from it are recorded on the
  instance and applied on exec.
- The sandbox-off knobs (`PUPPETEER_DANGEROUS_NO_SANDBOX`,
  `ELECTRON_DISABLE_SANDBOX`, …) are set only when the check fails.
  `DEVSANDBOX_BROWSER_SANDBOX=1` says it works.
- On root plans a browser wrapper (`devsbd` as `chrome`, `chromium`, …)
  starts the browser at a non-root euid in a single-uid user namespace
  over container root: no account needed, files still come out root's
  (which is you on rootless docker). A separate browser uid was rejected
  (unwritable profile dirs, foreign-owned files).

## Consequences

- Unprivileged user namespaces widen kernel attack surface on docker, the
  same trade podman's default makes.
- The Playwright library still adds `--no-sandbox` by default; agents are
  told (the devsandbox-cli skill) to opt in through
  `DEVSANDBOX_BROWSER_SANDBOX` and `DEVSANDBOX_CHROME`.
- Instances created before the check get no tool env until rebuilt.

Source: the identity plan, steps 7–10.
