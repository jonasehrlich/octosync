# octosync

Synchronize GitHub organization members to local user accounts on a Linux system.

## Setup

- Create a GitHub App with the following permissions at
  <https://github.com/organizations/your-org/settings/apps>:
  - Do not configure User authorization
  - Disable Webhook events
  - Permissions:
    - Organization members: Read-only
- Create a private key for the GitHub App and save the `.pem` file to the project directory.
- Download the application for your platform from the
  [releases](https://github.com/jonasehrlich/octosync/releases).
- Create a cron job to run the application at your desired interval, e.g., every hour:

```sh
0 * * * * /path/to/octosync sync --org <org-name> --app-id <app-id> --private-key /path/to/private-key.pem
```

## Installation

Install the latest version from the
[releases](https://github.com/jonasehrlich/octosync/releases/latest) page or build from source using
the instructions below.

## Run

```sh
octosync sync --org <org-name> --app-id <app-id> --private-key <private-key.pem>
```

### Group management

Add every synced user to a Linux group with `--group <linux-group>`, or map a GitHub team to a Linux
group with `--group <gh-team-slug>:<linux-group>`. Both forms can be passed multiple times.

```sh
octosync sync --org <org-name> --app-id <app-id> --private-key <private-key.pem> \
  --group developers --group backend-team:backend
```

Linux user groups are created if they are missing. Mapped GitHub teams are checked against the org's
team list. A mapped team that does not exist in the org is skipped with a warning: the Linux group
is not created and no longer assigned to any synced user.

octosync fully manages the supplementary groups of synced users. On every sync they are replaced
with the groups derived from the `--group` arguments, so memberships added through other channels
are removed.

### Departures disable accounts instead of deleting them

When a member leaves the org, octosync disables the account by setting its shadow expiry. It also
removes every authorized key, removes the crontab and queued `at` jobs, ends running sessions and
processes, and removes supplementary groups. The account and home directory remain on the machine.
The users database keeps a departure record, so a member who rejoins can be restored to the same
account with their files, UID and GID intact.

Sessions are ended through logind, which logs the user out cleanly and tears down their session
scopes. On a machine without logind, such as a container, the SIGTERM/SIGKILL process sweep is the
whole mechanism and the missing system bus is not reported as an error.

Accounts that have been disabled for longer than the retention period (180 days by default,
configurable with `--purge-after-days`) are eligible for purge at the end of each sync. Purging
permanently deletes the account and home directory without an archive. It runs only when the stored
departure date and the account's shadow expiry are both old enough, and the member is absent from
the fetched member list. The purge can also be run explicitly:

```sh
octosync purge --org <org-name> --app-id <app-id> --private-key <private-key.pem>
```

After permanent deletion, the users database still remembers the member's UID and GID. A member who
rejoins later receives the same IDs with a new, empty home directory.

## Development

When developing on Linux, you can run the application directly using `cargo run`. For
cross-compilation to other platforms, use `cargo-zigbuild` as described below.

### Cross-compilation

Install [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) using

```sh
cargo install cargo-zigbuild
```

Build for the target platform using

```sh
cargo zigbuild --target <target-triple>
```

List all target triples using

```sh
rustup target list
```

or

```sh
rustc --print target-list
```
