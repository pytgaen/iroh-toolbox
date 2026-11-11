# Security policy

## Supported versions

iroh-toolbox is pre-1.0. Security fixes are released for the latest published
version only.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/pytgaen/iroh-toolbox/security/advisories/new).
Do not open a public issue, and do not include real relay tokens, tickets,
pairing codes or private addresses in the report.

Include the affected version (`iroh-toolbox --version`), the command and
connection mode (direct, relay, code pairing), and the steps to reproduce.

You should receive an acknowledgement within 7 days. Once a fix is available,
it is published in a release and the advisory is disclosed with credit, unless
you prefer to remain anonymous.

## Scope

The [security model](docs/reference/security.md) describes what the toolbox
protects and what it does not. Notably, a relay token does not authorize access
to an individual peer's service, and anyone holding a manual ticket can attempt
a connection. Reports about these documented limits are welcome as hardening
suggestions in public issues.
