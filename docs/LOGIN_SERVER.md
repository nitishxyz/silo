# Login server

Status: built (P8-6); the "As built" notes say where it differs from the design. Fills the room `docs/PHASE7.md` §3.7 left.

## 1. What it is for

One machine, several people, each as their own unix account, from one install:

1. A shared Linux box: everyone reaches their own Silo server through one address.
2. `root@host` or a service account, without pairing it separately.
3. An admin lets someone in without that person first SSHing in to run `silo pair`.
4. A person's server starts when they connect. No `loginctl enable-linger`, no user service.
5. Sharing by name: "let ana view this session" (`Who::Account` grants, refused until now).

A machine with one user does not need it: `silo up` and pairing are enough.

## 2. Shape: a broker, not a proxy

`silo login-server` runs as root. It never carries a session's bytes. It identifies a device, decides which
account it may use, makes sure that account's server runs, has that server admit the device, and tells the
device where the server is. The device then talks to the account's server directly, as a paired device.

```
device ──(iroh, ALPN silo/login/1)──▶ login server (root)
                                         │ 1. device key → account (accounts.json)
                                         │ 2. start `silo server` as the account, if not running
                                         │ 3. login.admit {device, name, account} on its unix socket
device ◀── {account, ticket of the account's server} ──┘
device ──(iroh, ALPN silo/ctl/1)──▶ the account's server      (a device grant: nothing new on this path)
```

Why this and not PHASE7's forwarding socket: the root daemon stays small (no streams, no terminal data, nothing
to keep up with the protocol), every stream keeps its existing path and checks, and the account's server keeps
working when the login server stops, restarts or upgrades.

- **Identity** is the device's key (its iroh endpoint id), proved by the connection itself. No passwords.
- **The account's server** is that person's ordinary server: `silo` on the machine and a remote device see the
  same sessions.
- Each account's server needs its own way out (relay or a reachable UDP port), as every Silo server does.

**As built:** `crates/silo-login` (`broker.rs`); the exchange is one `silo_proto` frame each way on one bi stream:
`CONTROL_HELLO` holding `LoginRequest` (at most 8 KiB, within 10 s), then `CONTROL` holding `LoginAnswer`.

## 3. Accounts file

`/etc/silo/accounts.json` (root, `0600`). Ships empty: nobody gets in.

```json
{
  "version": 1,
  "accounts": {
    "alice": { "devices": ["7f3a…"] },
    "root":  { "devices": ["7f3a…"] },
    "*":     { "devices": ["91bc…"] }
  },
  "defaults": { "7f3a…": "alice" }
}
```

- `devices`: endpoint ids (64 hex), each optionally `"id name"` for a label.
- `"*"`: its devices may use any regular account (uid ≥ 1000) whether or not the file names it. Root and system
  accounts are always listed by name.
- With no account asked for: the device's `defaults` entry, else its only account, else an error naming the
  choices.
- The daemon rewrites the file when an invite is redeemed or `silo login-server allow|deny` runs (atomically,
  keeping unknown keys). `SIGHUP` or `silo login-server reload` re-reads it; devices that lost an account are
  revoked on that account's server (`login.revoke`).

**Refused always:** an account that does not exist, one whose shell is `nologin` or `false`, a locked one, any
non-root account while `/etc/nologin` exists.

**As built** (`policy.rs`, `users.rs`): a bad entry fails the whole file (a reload then keeps the policy in force).
As root the file must be owned by root and not group/world writable; it is read and replaced without following
symlinks. "Regular" is uid 1000..60000. A `defaults` entry the device may no longer use is skipped, not a lockout.
Every rewrite re-reads the file first, so hand edits not yet reloaded survive. On reload (and `allow`/`deny`)
the devices revoked are those the old file named for an account plus those admitted since the daemon started (a
`*` device's accounts are known only that way); a server not running is started to revoke. Failed logins are
limited to 5 a minute per device.

## 4. Letting someone in

```sh
sudo silo login-server invite alice [--ttl 1d]     # prints a silol1… ticket (and a QR)
sudo silo login-server allow alice <device-id>      # a device whose id you already know
sudo silo login-server deny  alice <device-id>
sudo silo login-server list
```

- A **login ticket** is `silol1` + base32 of `{addr, secret?, account?}`: how to reach the login server, an
  invite's secret (single use, memory only, 1 day by default) and the account it is for.
- The person adds the host in Silo (Add Host, or `silo hosts add <ticket>`). The client dials the login server,
  redeems the invite (its device is written into the accounts file), and saves the host as an ordinary paired
  host plus where its login server is.
- A ticket without a secret (`silo login-server ticket`) is the machine's address: any device already in the
  file uses it, `account@` in front picks the account (`silo hosts add root@silol1…`).

**As built:** `invite` prints a QR only with `--qr`; invites last at most 7 days. `LoginRequest.invite` is the
secret in lowercase unpadded base32. Invites and `reload` need the daemon (they reach it on `admin.sock` in its
state dir, root or its own uid only, one JSON line each way); `allow`, `deny` and `ticket` fall back to the
file (and to revoking from the CLI process) when it is not running; `list` reads the file. The host is saved as
`account@hostname` unless named.

## 5. Connecting

1. The client dials the login server: `LoginRequest {protocol, account?, invite?, name, version}` on one stream.
2. The login server picks the account, starts its server if `<runtime>/silo.sock` does not answer, and calls
   `login.admit`.
3. The account's server gives the device a device grant (`control` on the server, label = the device's name,
   `created_by: "login server"`) if it has none, and answers with its pairing-free ticket (its address).
4. The client gets `LoginReply {account, ticket, server_id}`, dials the server and says hello as a paired device.

Later connects go straight to the account's server. When it does not answer (rebooted, never started), the
client goes through the login server again; that starts it.

**Starting a server as an account:** fork, `initgroups`, `setgid`, `setuid`, a clean environment (`HOME`, `USER`,
`LOGNAME`, `SHELL`, `PATH`, `XDG_RUNTIME_DIR` when `/run/user/<uid>` exists), `setsid`, exec `silo server
--adopt`. It is not a child the login server waits on: it outlives it. No PAM session in this version (§9).

**As built** (`spawner.rs`): the socket is `<silo home>/server.sock`, the home computed as
`silo_proto::paths` would with only the account's `HOME` set (`~/.local/state/silo`); a server the user runs with
another `SILO_HOME`/`XDG_STATE_HOME` is not found. The socket's peer must be the account's uid. `initgroups` is
`getgrouplist` in the parent plus `setgroups` in the child (only async-signal-safe calls after fork). The login
must come from the device key's own endpoint (the CLI binds it for the exchange). `LoginReply.ticket` is the
server's secret-less address ticket (`siloa1…`, `silo_transport::encode_addr`); `server_id` is its endpoint id.
The direct-dial fallback through the login server is `silo_client::login::{not_answering, relogin}`, used by the
CLI (`silo_cli::connect_remote`) and the app (`conn.rs`: once per connect attempt, then one retry, before the
usual backoff). Unprivileged servers are started with `SILO_LOGIN_TRUST_OWN_UID=1` (below).

## 6. The account's server

- A unix peer with **uid 0** that says `role: "login"` in its hello is the login server
  (`Principal::Login`). It may call `login.admit`, `login.revoke`, `server.info` and `ping`, nothing else. Root
  saying anything else is refused as before.
- `login.admit {device, name, account}`:
  - `account` is this server's user: ensure the device grant, answer with the address.
  - another account: only if a `Who::Account {name: account}` grant exists. The device gets a device grant with
    that grant's scope and rights, `created_by_grant` = the account grant, so revoking or expiring the account
    grant removes it and closes its connections.
- `login.revoke {device, account?}` removes the grants `login.admit` made for that device (never one made by
  `silo pair`).
- `grant.create` with `kind: account` is no longer refused. Without a login server on the machine the grant does
  nothing.

**As built:** a unix peer of another uid is `Peer::Root` (uid 0 only; others are refused at accept) and gets one
control stream, admitted only with `role: login`. `role: login` from the server's own uid is admitted only on
root's own server or with `Config::trust_own_login` (env `SILO_LOGIN_TRUST_OWN_UID=1`; tests and `--unprivileged`;
it skips approval for pairing, nothing else an owner lacks). No new `Grant` field: grants `login.admit` makes have
`created_by: "login server"` (only the server sets `created_by`; `silo pair` devices have it empty), and a device
admitted as another account has `created_by_grant` = the account grant, its scope, its rights capped at
`control`, and its expiry. `remove_where` now also removes account grants with their creator, and device grants
whose creator is a removed account grant. A device that already holds a grant (paired, or its own account's)
keeps it. Such a device's hello carries `granted`. `grant.create {kind: account}` takes the account's name in
`label` (no protocol change).

## 7. Sharing by name

`silo grant --account ana --session work --rights view` on alice's server. Ana's client asks the login server
for alice's server as ana (`LoginRequest.server_of: "alice"`); the login server checks ana's device is one of
ana's, then calls alice's server `login.admit {device, name, account: "ana"}`. Ana's Silo shows the host with
what the grant covers, as a share link does.

**As built:** `server_of` is in the broker and `silo_client::login::login`. On alice's machine:
`silo share [SESSION] [--block B] --account ana [--rights view|interact] [--ttl 7d]` makes the account grant
(`grant.create {kind: account, label: "ana"}`; no ttl: until revoked) and prints
`ana can now add this machine with: silo hosts add ana@silol1… --server-of alice` (the ticket from the login
server's admin socket, else "the login ticket from your admin"; with `--host`, from that host's stored login
server). `silo share --list`/`--revoke` cover account grants too. Ana runs
`silo hosts add <silol1…|ana@silol1…> --server-of alice [--name N]`:
she logs in as her own account, the host is saved as `alice@hostname` with `login.server_of`, and relogins ask the
same. The app's Add Host has no `server_of` field yet.

## 8. Running it

- `packaging/linux/silo-login.service` (root; `ExecStart=silo login-server run`, `ExecReload=kill -HUP`), state
  in `/var/lib/silo-login` (its identity key), policy in `/etc/silo`.
- `sudo silo login-server install` writes the unit and the empty accounts file and enables it.
- Flags: `--accounts`, `--state-dir`, `--relay`, `--port`, `--no-mdns`.
- **Not root** (`--unprivileged`, for tests and trying it out): only the daemon's own account, started without
  changing user.

**As built:** every admin command also takes `--accounts`/`--state-dir`. `install` writes
`/etc/systemd/system/silo-login.service` for the running binary (`KillMode=process`, so a restart of the login
server leaves the accounts' servers, which stay in its cgroup) and creates the accounts file `0600`. With
`--unprivileged` the server it starts must be one it started (or one run with `SILO_LOGIN_TRUST_OWN_UID=1`).

## 9. Not in this version

- PAM sessions and systemd-logind (resource limits, `XDG_RUNTIME_DIR` creation); FreeBSD.
- SSH keys or Tailscale identities as principals; unix peers (`uid:1000`) as principals.
- Socket activation.
- A web or iOS client's login flow (the same `silo/login/1` exchange).

## 10. Protocol

- `silo-transport`: `LOGIN_ALPN = b"silo/login/1"`, `LoginTicket` (`silol1`), `Net::serve_login`, `Net::dial_login`.
- `silo-proto/src/login.rs`: `LoginRequest`, `LoginReply`, `LoginError`, `LoginAdmitParams`, `LoginAdmitted
  {ticket, grant_id}`, `LoginRevokeParams`.
- Methods `login.admit`, `login.revoke` (need: `Need::Login`); `ClientRole::Login`; feature `login`.
- Additive: an older server answers `method_not_found`, and the login server says the account's server is too
  old.

**As built:** `Net::serve_login` returns a `LoginIncoming`; `LoginTicket::parse` takes `account@silol1…`;
`silo_transport::valid_account` is the one user-name check.

## 11. Tests

- Policy: account choice, `*`, defaults, refusals, rewrite keeps unknown keys.
- Broker end to end with an in-process account server and a spawner that starts it in a temp home
  (`--unprivileged` path): invite → device in the file → `login.admit` → the device connects as a paired device.
- Server: `Principal::Login` reaches only `login.*`; an account grant admits a device with the grant's scope and
  goes with the grant; `login.revoke` leaves `silo pair` devices alone.
- As root (Docker, not in CI): two accounts, each started as its user.

**As built:** `silo-login/src/{policy,users,install,spawner}.rs` unit tests, `silo-login/tests/broker.rs`
(unprivileged, in-process account server), `silo-server/tests/login.rs`, `silo-server/src/grants.rs`, ticket
round trips in `silo-transport/src/login.rs`. The root test (two accounts, each started as its user) is not
written.
