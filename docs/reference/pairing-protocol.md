# Code pairing protocol

`send`, `chat serve` and `pair` create a one-use code of words, and the
`code:` targets of `receive`, `chat connect` and `pair` join with it. A code is
how two machines that are not paired meet; paired machines can also use
`peer:NAME` targets ([peers](peers.md)). No mailbox or other server operated
by this project is involved: the machines meet through iroh's relays and
address lookup, then authenticate each other with SPAKE2. Only a ticket and the
joiner's endpoint ID (files, chat) or a signed machine record (`pair`) crosses
the pairing connection; file contents and chat messages travel afterwards over
the normal iroh connection.

```console
A$ iroh-toolbox send ./my-folder
Code: amber-falcon-tide-rocket-maple-orbit
On the receiving machine:
  iroh-toolbox receive code: --output ./received
  then type the code at the prompt (it stays out of the process list)

B$ iroh-toolbox receive code: --output ./received
Code:
```

On a terminal, the code appears on stderr as `Code: WORDS`; when stdout is not a
terminal (or with `--no-hints`), stdout also receives the bare words, for
scripts. `chat serve` adds `(single-use: first admitted participant takes the
session)` to its `Code:` line.

## Code

A code has six words by default (`--code-words` accepts 6 to 32), drawn
uniformly from the [EFF large wordlist](https://www.eff.org/dice) (7776 words,
CC BY 3.0); the four hyphenated entries are skipped, leaving 7772 words of about
12.9 bits each. Codes are case-insensitive and accept `-` or spaces between
words. A misspelled word is rejected locally, before any network activity.

| Part | Words | Entropy | Role |
| --- | --- | --- | --- |
| Rendezvous `R` | 1–3 | ≈ 38.8 bits | Locates the creator; treated as public |
| Password `P` | 4–6 | ≈ 38.8 bits | SPAKE2 password; only online guessing is possible |

`R` always has three words: `--code-words` adds words to `P` only.

## Rendezvous

The creator **A** derives a temporary iroh secret key from `R`:

```text
seed = HKDF-SHA256(ikm = R, salt = "iroh-toolbox/pairing/v1/rendezvous", info = "secret-key")
```

A binds a temporary endpoint with this key and ALPN `iroh-toolbox/pairing/1`.
This endpoint is relay-only: it has no UDP socket, so no direct address is
advertised or probed. The key derives from public words and iroh exchanges
direct addresses at connection time, before any `hello`; a direct-capable
endpoint would hand its IP addresses to anyone who derives the ID. The pairing
exchange is a few kilobytes, so the relay costs nothing. The ticket that follows
points at A's regular endpoint, which keeps direct paths.

The joiner **B** derives the same endpoint ID and connects with the ID alone,
retrying until the pairing deadline while A registers. B does this from a
temporary endpoint that is also relay-only (and, with public relays, resolves
A's rendezvous ID through n0's DNS). Anyone who knows `R` can squat the
rendezvous ID and answer in A's place, and B's direct addresses would reach
that squatter before SPAKE2 fails. B binds the temporary endpoint
concurrently with the endpoint it will transfer from, uses it for the whole
pairing exchange, and closes it when the pairing ends. A random key makes
its endpoint ID `idB`. The transfer endpoint keeps its direct transports and
is announced to A only inside the authenticated channel (see
[Payload](#payload)); B then connects to A's ticket directly, as before.

| Relay setting | How B reaches A |
| --- | --- |
| Public relays (default) | A publishes its relay URL, and no IP address, through n0's DNS (pkarr); B resolves it |
| `--relay URL` | Both use the selected relay; the ID and URL suffice, nothing is published |
| `--direct-only` | Not supported: codes need a relay to meet |

Anyone who knows `R` can derive the same key, so the TLS identity of the
rendezvous endpoint proves nothing. All authentication comes from SPAKE2. A
squatter that publishes a fake record only causes a failed attempt.

## Handshake

Frames are JSON objects with a `type` field, unknown fields rejected, prefixed
by a 16-bit big-endian length (at most 4 KiB) on one bidirectional stream. Byte
values are hex strings. Both sides hash every frame, in stream order, into a
running SHA-256 transcript.

```text
1. B → A  hello    { version: 1, purpose, verify, pake: SPAKE2_B(P, idA, idB) }
2. A → B  hello    { version: 1, purpose, verify, pake: SPAKE2_A(P, idA, idB) }
          K = SPAKE2 shared key; T = transcript after the two hellos
          keys = HKDF-SHA256(K, salt = T, info = confirm-host | confirm-joiner | sas | done)
3. A → B  confirm  { mac: HMAC(confirm-host, T) }
4. B → A  confirm  { mac: HMAC(confirm-joiner, T) }
```

- `idA` is the rendezvous endpoint ID; `idB` is the ID of B's temporary
  pairing endpoint, authenticated by iroh's TLS on A's side. A relaying attacker
  must connect to A with its own endpoint, so A's `idB` differs from B's and the
  keys diverge. `idB` is not the endpoint B transfers from.
- `purpose` (`files`, `chat` or `pair`) and `verify` must match on both sides. `pair` always sets `verify`: a hello with the `pair` purpose and no `verify` is refused. The joiner never adopts the host's setting: each machine uses its own, and a mismatch is refused on both sides (`--no-verify` on both machines, or on neither).
- SPAKE2 elements are rejected unless they decode to torsion-free, non-identity
  points; the `spake2` crate does not check this itself
  ([review](https://github.com/pytgaen/iroh-toolbox/blob/main/notes/spake2-review.md)).
- **One attempt per code:** A serves the first connection that sends a hello,
  whatever its outcome, then closes the rendezvous. A wrong password succeeds
  with probability about 2⁻³⁸.

## Confirmation number

By default, each terminal shows its own 6-digit number and asks for the other
machine's. If the people read their numbers to each other
over a channel the attacker cannot alter, a correctly typed number shows that
nobody sits between the machines, even if the code was overheard. A commitment
prevents an attacker who knows the code from trying values until the numbers
match:

```text
5. A → B  commit   { commitment: SHA-256("iroh-toolbox/pairing/v1/commit" || nA) }
6. B → A  nonce    { nonce: nB }
7. A → B  reveal   { nonce: nA }        B checks the commitment
          h = HMAC(sas, T || nA || nB)
          number of A = (bytes 0–7 of h as u64) mod 1 000 000
          number of B = (bytes 8–15 of h as u64) mod 1 000 000
8. A → B  ready    after B's number was typed correctly on A
9. B → A  ready    after A's number was typed correctly on B, and A's ready
```

Each machine shows its own number and compares the typed one with the other
machine's number, as it computed it, in constant time and with spaces ignored.
The person cannot copy their own screen: the expected number is shown only on
the other machine.

Neither machine chooses the numbers. A commits to `nA` before it sees `nB`, and
B picks `nB` while it sees only the commitment. A man in the middle runs one
session with each machine; in each, the value it does not control is fixed
before it can react, so the number read aloud from the other session is the one
a machine expects with probability 10⁻⁶, checked independently on each side.
Three wrong numbers (typing mistakes only: the expected number is fixed), an
empty answer, an interruption or the deadline cancel the pairing on both
machines.

A is always the machine that created the code:

| Commands | A (commits first) | B |
| --- | --- | --- |
| `send` → `receive code:…` | Sender | Receiver |
| `chat serve` → `chat connect code:…` | Creator | Joiner |
| `pair` → `pair code:…` | Host | Joiner |

Users have nothing to choose: each screen shows its number, and each person
types the other's. `send`, `receive`, `chat serve` and `chat connect` ask for it
unless `--no-verify` is given on both machines; `pair` always requires it and
has no such option. Without a terminal and without `--no-verify`, the command
fails before any network activity. In `chat serve`, the number covers only the code: a paired
machine joining through the peer path proves its machine key instead. The
[security model](security.md#confirmation-number-6-digits)
explains what the check detects and what it cannot.

## Payload

For `files` and `chat`:

```text
10. A → B  payload  { ticket }     blob ticket (files) or endpoint ticket (chat)
11. B → A  payload  { endpoint }   B's transfer endpoint ID: 64 lowercase hex digits
12. A → B  done     { mac: HMAC(done, "host" || transcript) }
13. B → A  done     { mac: HMAC(done, "joiner" || transcript) }
```

The announcement is sent after key confirmation and, with the confirmation
number, after both `ready` frames. It is part of the transcript, so both `done`
MACs cover it, and A uses it only after verifying B's `done`. A key that is not
64 lowercase hex digits encoding a valid public key ends the pairing. The
receiver validates the ticket (collection format, selected relays). The sender
then serves blobs only to the announced endpoint, never to `idB` or to the
connection that carried the pairing; the chat creator accepts only the
announced endpoint, and everything that pins the paired identity (reconnection,
resume, file offers in the chat) uses it. A resumed reception does not pair
again: it reuses the key saved in its partial directory, whose public key is the
one announced on the first run. When `chat serve` also opens a chat slot for paired machines, the
first participant admitted on either path takes the session: a code completed
after a paired machine was admitted is refused, and once either path admits,
the rendezvous closes. A failure closes the connection with a reason code (10
different command or `--no-verify` setting, 11 wrong code, 12 confirmation
declined, 13 name taken), so the other machine reports the cause even if the
closing frame is lost.

## Machine pairing (`pair`)

The `pair` purpose exchanges signed machine records instead of a ticket.
Rendezvous, SPAKE2, key confirmation and the confirmation number are unchanged,
and the confirmation number is mandatory. After the two `ready` frames:

```text
10. A → B  payload  { peer: { machine: MA, salt: sA, name: nameA, signature: Sign_MA(S_A) } }
11. B → A  payload  { peer: { machine: MB, salt: sB, name: nameB, signature: Sign_MB(S_B) } }
12. A → B  done     { mac: HMAC(done, "host" || transcript) }
13. B → A  done     { mac: HMAC(done, "joiner" || transcript) }

S_X = "iroh-toolbox/pair/v1" || len(role_X) || role_X || T || machine || salt || len(name) || name
T   = transcript after the confirmation-number ready frames
```

`role_X` is `host` or `joiner`; `machine` is the 32-byte public key and `salt`
the 10-byte address salt (text fields are z-base-32 in the frame); both `len`
prefixes are one byte, names being at most 32 bytes. A `payload` carries exactly
one of `ticket`, `endpoint` or `peer`; a frame with two of them, or any other
field, is refused. `pair` announces no endpoint: B pairs from the same
relay-only temporary endpoint and the records are unchanged.

- **The signature proves** that each side holds the machine key it announces,
  for this session only: a paired machine cannot register someone else's key,
  and a record cannot be replayed in another session. Signatures use Ed25519
  strict verification. A record carrying the receiver's own key is refused.
- **Order on each machine:** check the other's record (name rules, signature),
  take the lock on the peer book, settle the local name (asking for another one
  on a conflict), and only then send `done`. There is no access question. A
  machine writes its entry only after verifying the other's `done`; the host
  closes the connection with code 14 (stored) after its write, which tells the
  joiner that the host's write succeeded. A missing 14 is reported as a warning
  on the joiner, never as a failure.
- **A machine does not send `done` when less than 5 seconds remain** before the
  deadline: the other could not store its entry in time and would leave a
  one-sided pairing. Nothing is stored.
- **Reasons:** 13 name taken (the local name belongs to another machine and
  nobody offers another; the pairing aborts before `done`, nothing is stored on
  either side), 14 stored (host closing after a successful write, not an
  error), plus 10 to 12 as above.

## Deadlines

`--pairing-timeout` bounds registration, lookup, SPAKE2, confirmation and
payload. It defaults to 300 seconds when a confirmation number is read (two
people answer questions: `pair`, and `send`, `receive` and `chat` unless
`--no-verify`) and to 120 seconds with `--no-verify`. A connection that sends no hello within the
connection timeout is ignored without consuming the code.

## Threats and limits

| Threat | Outcome |
| --- | --- |
| Online password guessing | One try per code, about 2⁻³⁸ |
| Offline brute force | A recorded SPAKE2 exchange does not let anyone test passwords offline |
| Relay operator | Sees both endpoint IDs, the IP addresses they connect from and the volume; learns no long-term identity and no content |
| DNS operator (public relays) | Sees the temporary rendezvous ID and A's relay URL; no IP address |
| Third party that connects to the rendezvous | Learns none of A's IP addresses (relay-only endpoint); may burn the attempt with a bogus `hello` |
| Squatting `R` | Denial of service: the attempt fails, create a new code. The squatter also learns B's temporary pairing endpoint ID, its home relay and the `hello`; no IP address (B pairs from a relay-only endpoint) and not B's transfer endpoint, which is announced only after SPAKE2 succeeds. It does not learn words 4–6. `R` has about 470 billion values, so squatting active codes at random is impractical |
| Code overheard | Detected by the confirmation number (10⁻⁶); with `--no-verify`, the eavesdropper may pair first |
| Channel carrying the code can be altered | Not detected if the numbers are compared over that same channel |
| Code from an unverified source | You pair with whoever created it; the confirmation number cannot detect it (see [what none of them prove](security.md#what-none-of-them-prove)) |
| `pair` record naming someone else's key | Rejected: the record is signed with that key over this session's transcript |
| `pair` with an impostor | Possible if the code came from the impostor; it can join the peer services you run whose `--allow` does not exclude it. Pairing starts no service |
| Chat code disclosed before use | Its holder may join first, even after `peers rm`; single-use stops a second admission, not the first. Restart `chat serve`, or use `chat serve --peers-only` |
| Two users drawing the same `R` | Negligible; the attempt fails, retry |

!!! warning "No independent audit"

    The pairing protocol and the `spake2` crate it relies on have not been
    audited independently. The maintainer review and its mitigations are in
    [`notes/spake2-review.md`](https://github.com/pytgaen/iroh-toolbox/blob/main/notes/spake2-review.md);
    an outside review is planned before 1.0. Do not rely on iroh-toolbox where a
    failure would be serious.

The `spake2` crate is licensed MIT OR Apache-2.0.
