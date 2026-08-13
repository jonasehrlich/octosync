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
0 * * * * /path/to/octosync sync --org \<org-name\> --app-id \<app-id\> --private-key /path/to/private-key.pem
```

## Installation

Install the latest version from the
[releases](https://github.com/jonasehrlich/octosync/releases/latest) page or build from source using
the instructions below.

## Run

```sh
octosync sync --org \<org-name\> --app-id \<app-id\> --private-key \<private-key.pem\>
```

### Home directory archives

Before a user is deleted, their home directory is archived to
`<data-dir>/home-archive/<user>-<uid>-<timestamp>.tar.gz`. The archive directory and the
archives are owned by root with permissions `700` and `600` respectively, so only root can
access them. If archiving fails, the user is not deleted and the deletion is retried on the
next sync.

Sockets and other special files cannot be stored in a tar archive and are skipped with a
warning. Only the three most recent archives are kept per user, older ones are pruned after
a successful archive. In dry-run mode, archiving is neither performed nor validated.

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
cargo zigbuild --target \<target-triple\>
```

e.g:

```sh
cargo zigbuild --target aarch64-unknown-linux-gnu
```
