# Environment probe and test contract

`scripts/environment-probe.py` is the minimum, credential-free probe for the
AGS environment contract. It uses only the Python standard library, starts no
subprocesses, makes no network requests, and never prints arbitrary environment
variable values.

The probe is intentionally observational. It reports the host facts visible to
the process; it does not modify or conceal them. AGS can run it before and after
applying an environment profile and compare the reports.

## Run the probe

```sh
python3 scripts/environment-probe.py
```

The command writes one JSON document to stdout and exits zero. The stable
top-level fields are:

- `schema_version`: currently `ags-environment-probe-v1`;
- `ok`: whether an expected profile matched, or `true` when none was supplied;
- `facts`: collected OS, process, time, locale, WSL, and PTY facts;
- `comparison`: `null` without `--expected`, otherwise the comparison result.

The collected facts cover:

- OS release, architecture, `uname`, hostname, effective user and groups;
- logical cwd, `PWD`, home from Python and `HOME`;
- current UTC time, `TZ`, local offset/name, IANA timezone source, and DST flag;
- active C locale categories, locale environment, and encodings;
- presence-only carrier variables such as `WSL_*`, `SSH_*`, cloud, container,
  Windows, and CI markers, plus presence-only Windows/WSL path markers;
- `/proc` availability, PID/PPID/PID 1, and bounded WSL marker checks;
- stdin/stdout/stderr TTY state, device, dimensions, `TERM`, and `COLORTERM`.

Values of carrier variables are never emitted. The probe does not enumerate
unknown environment variables, read credentials, query cloud metadata, inspect
network interfaces, or contact a DNS or HTTP endpoint.

## Compare with an expected profile

`--expected` accepts a JSON file. The expected document is a recursive subset
of `facts`: omitted fields are ignored, while every supplied field must exist
and match exactly, including JSON types. A mismatch exits `1`; invalid or
unreadable input exits `2`. Mismatch reports contain only a fact path and reason,
so an accidentally sensitive expected value is not echoed.

Linux or WSL example:

```json
{
  "os": {
    "system": "Linux",
    "machine": "x86_64"
  },
  "proc": {
    "available": true
  },
  "wsl": {
    "detected": true
  }
}
```

```sh
python3 scripts/environment-probe.py --expected /path/to/wsl-profile.json
```

macOS example:

```json
{
  "os": {
    "system": "Darwin",
    "machine": "arm64"
  },
  "wsl": {
    "detected": false
  }
}
```

The same input can be passed on stdin:

```sh
printf '%s\n' '{"os":{"system":"Linux"}}' |
  python3 scripts/environment-probe.py --expected -
```

Do not put current time, PID, PPID, terminal device number, or other expected-to-
change facts in a persistent profile. Test those fields by type and invariant in
the AGS test runner.

## Required test matrix

Run the probe through the exact launch path used by the agent, not only from an
interactive shell. Every supported backend must cover these scopes:

| Scope | Linux | WSL2 | macOS |
|---|---:|---:|---:|
| AGS parent process | required | required | required |
| shell child | required | required | required |
| grandchild after `exec` | required | required | required |
| local MCP stdio server | required | required | required |
| non-TTY pipes | required | required | required |
| allocated PTY | required | required | required |

For each scope, retain the profile ID, backend kind, probe schema version,
exit code, and SHA-256 of the JSON evidence. Evidence containing cwd, home,
username, hostname, or terminal device is private diagnostic data and must not
be published without explicit authorization.

## Acceptance tests

1. **Schema and JSON:** run without `--expected`; stdout parses as one JSON
   object, `schema_version` matches, `ok` is true, and exit status is `0`.
2. **Positive subset:** compare a profile containing the observed OS and
   architecture; `comparison.matched` is true and exit status is `0`.
3. **Negative subset:** expect an impossible OS value; output remains valid
   JSON, the mismatch path is `$.os.system`, no expected value is echoed, and
   exit status is `1`.
4. **Bad input:** use malformed JSON and a non-object JSON value; both produce a
   JSON error envelope and exit status `2`.
5. **Secret canary:** set `AGS_PROBE_SECRET_CANARY` to a unique value, run the
   probe, and assert that neither its name nor value appears in stdout.
6. **Carrier environment:** set one known marker such as `WSLENV` and verify
   only `environment.carrier_markers.WSLENV=true` is emitted, never its value.
7. **WSL markers:** on a normal WSL2 backend, `wsl.detected` must be true and at
   least one environment or system marker must be true. Under a profile that
   promises to hide WSL, the expected profile must require every exposed WSL
   marker to be false; any remaining true marker rejects launch.
8. **Native Linux and macOS:** `wsl.detected` must be false. The native OS,
   architecture, uname, home, cwd, and user facts must match the backend's
   declared capability report.
9. **Time and locale:** run with controlled `TZ`, `LANG`, `LC_ALL`, and
   `LC_TIME`; verify the environment fields, active locale, IANA name, local
   offset, and name agree. Repeat around a pinned DST transition in a test
   backend. This probe does not virtualize the clock or pin tzdata, so AGS must
   supply those mechanisms before claiming time equivalence.
10. **Process inheritance:** compare parent, shell child, exec grandchild, and
    MCP reports. Stable identity, path, timezone, locale, carrier-marker, and
    terminal facts must match the profile. Dynamic PID fields must differ as
    expected.
11. **PTY:** piped execution must report `isatty=false`. Execution through the
    same PTY broker used by the agent must report `isatty=true`, consistent
    dimensions for all intended streams, and the declared `TERM`.

Passing this probe proves observational equivalence only for the fields in
`ags-environment-probe-v1`. It does not prove syscall-level concealment, network
identity, resource virtualization, cloud-metadata isolation, or resistance to a
malicious native program. Those require separate versioned probes and backend
capabilities.
