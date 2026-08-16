# Changelog

## Unreleased

- Account lifecycle now separates member departure from permanent account deletion.
  ([#25](https://github.com/jonasehrlich/octosync/pull/25),
  [#28](https://github.com/jonasehrlich/octosync/pull/28))
  - Departed members are disabled instead of immediately deleted. octosync blocks new logins, ends
    sessions and processes, removes scheduled work and supplementary groups, and preserves the
    account and home directory.
  - Rejoining restores the account and its files.
  - Despite its name, the `delete` command uses the reversible departure flow.
  - Accounts disabled beyond the configurable retention period of 180 days by default are eligible
    for purge. Purge can run during sync or through the new `purge` command.
  - Purge permanently deletes the account and home directory without an archive. An interrupted
    account deletion resumes from its stored marker.
- Synchronization now protects against incomplete membership updates.
  ([#15](https://github.com/jonasehrlich/octosync/pull/15),
  [#17](https://github.com/jonasehrlich/octosync/pull/17),
  [#25](https://github.com/jonasehrlich/octosync/pull/25),
  [#28](https://github.com/jonasehrlich/octosync/pull/28))
  - A failed member update does not disable the member.
  - Membership data that would disable every stored account is rejected before accounts are changed.
- Account identity is preserved across departures, manual account deletion and rejoins.
  ([#19](https://github.com/jonasehrlich/octosync/pull/19),
  [#22](https://github.com/jonasehrlich/octosync/pull/22),
  [#25](https://github.com/jonasehrlich/octosync/pull/25))
  - Stored UIDs and GIDs are restored when an account is recreated.
  - IDs assigned to departed members cannot be reused by other members.
  - Unsafe login or ID collisions are rejected rather than modifying the wrong account.
- SSH authorized key handling now distinguishes routine synchronization from account departure.
  ([#15](https://github.com/jonasehrlich/octosync/pull/15),
  [#28](https://github.com/jonasehrlich/octosync/pull/28))
  - Routine synchronization fetches keys through the authenticated GitHub client and updates an
    octosync-managed block while preserving manually managed keys.
  - Existing keys remain in place when fetching fails.
  - Writes are atomic, enforce ownership and permissions, and cannot be redirected through symlinks
    or replacement of the `.ssh` directory.
  - Departure removes the entire `authorized_keys` file, including manually managed keys. Rejoining
    recreates it from the member's fetched GitHub keys.
- GitHub teams can now be mapped to Linux groups with `--group <gh-team-slug>:<linux-group>`.
  ([#20](https://github.com/jonasehrlich/octosync/pull/20))
- Synchronization migrates `users.json` to a versioned v2 format.
  ([#22](https://github.com/jonasehrlich/octosync/pull/22),
  [#25](https://github.com/jonasehrlich/octosync/pull/25),
  [#28](https://github.com/jonasehrlich/octosync/pull/28))
  - The v2 format records active members, departed members and members whose accounts were
    permanently deleted.
  - The original v1 file is backed up during the write as `users-v1.json`.

## v0.3.0

- Added a `--version` flag to display the current version of the application.

## v0.2.0

- Added group sync with the `--group` option.
- Added locking of the data directory to prevent concurrent access issues.

## v0.1.0

- Initial release of the project.
