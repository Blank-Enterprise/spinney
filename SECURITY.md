# Security policy

Spinney stores GitHub tokens in the macOS Keychain, acts as git's credential helper, and manages
SSH keys, so a vulnerability can expose access to people's repositories. Please report one
privately.

## Reporting a vulnerability

**Don't open a public issue.** Use GitHub's private vulnerability reporting instead:

1. Go to the [Security tab](https://github.com/SameetKeshav/spinney/security) and choose
   **Report a vulnerability** ([direct link](https://github.com/SameetKeshav/spinney/security/advisories/new)).
2. Describe the problem, how to reproduce it, and what an attacker could do with it.

Only the maintainers can see the report. We'll reply there, agree on a fix, and credit you in the
advisory unless you'd rather we didn't.

## Supported versions

Only the latest release gets security fixes. Spinney is early and updates often, so upgrade with the
install command in the [README](README.md#install).

## In scope

- Tokens or keys leaving the Keychain, or going to a host other than the space's own
  (see `creds::helper_response`).
- The credential helper answering requests it shouldn't.
- Git being run so that it reads attacker-controlled input as options or paths.
- File operations that reach outside the worktree.
- The install script or release artifacts being tampered with without the checksum check failing.
