# Connectors: a credential gateway for the agent's fetch

The agent calls external services as if it were calling them directly. A
gateway inside the runtime recognizes which service a request is for,
attaches that service's credentials, and keeps the session alive. The agent
never sees a token, a cookie or a key.

## Decisions

| | Decision |
| --- | --- |
| Login types | OAuth 2.1 (authorization code with PKCE, refresh), Mobile BankID (same-device autostart link, then cookies), API key |
| BankID scope | Identification only. Acting usually needs BankID signing, which isn't supported |
| Who starts a login | The agent, when it decides it needs one. No approval step beyond BankID itself and fixed OAuth scopes |
| How requests get credentials | By origin. Each connection lists the origins it serves; one connection per service |
| Where | In-process, behind the `fetch` capability. `fetch` is the only HTTP path for now |
| Secrets | A separate SQLite vault (`/data/vault.db`), encrypted at rest with a key from the Kubernetes Secret. The agent's `db` capability can't reach it |
| Setup | OAuth clients and API keys come from the Secret and config, never from chat |
| Sessions | OAuth refreshes. Cookie sessions get a configured keepalive request, for up to an hour by default |
| OAuth callback | `https://evalien.bushnet.diy/oauth/callback`, reachable over Tailscale. The browser needs Tailscale while authorizing |
| Observability | Connection state changes in the trajectory; per-connection request metrics in Prometheus |
| Later | Proton (needs its Bridge or its encrypted web API), BankID QR for desktop, signing |

## Architecture

```
 sandbox ── fetch(url) ──▶ fetch capability ──▶ Gateway::attach ──▶ network
                                │                     │
                                │                     ├── vault (tokens, cookies)
                                ◀── response ◀── Gateway::observe (401 → needs login)

 sandbox ── connections.connect("kivra") ──▶ Gateway::connect ──▶ login flow
                                                                    │
                     trajectory ◀── notice: "BankID: tap to sign in" ┘ ──▶ Matrix
```

- `connectors/` is a folder module that depends only on `domain`.
- The `fetch` capability (in `evaluation/`) sees the gateway through a domain port, so neither module imports the other. The composition root wires them.
- Login flows run as tasks in the runtime. Their notices go into the trajectory, and from there out through the chat relay like any other notice.

### The port

```rust
pub trait Gateway: Send + Sync {
    /// What to add to a request for `url`: cookies need the path, not just the origin.
    fn attach(&self, url: &Url) -> Attachment;
    /// Learn from how a request went, and return the connection's status afterwards.
    fn observe(&self, connection: &str, outcome: RequestOutcome) -> ConnectionStatus;
    fn connections(&self) -> Vec<ConnectionInfo>;
    fn skill(&self, connection: &str) -> Option<String>;
    /// Sign in if needed. Concurrent calls wait on the same login.
    fn connect(&self, connection: &str, timeout: Duration) -> Connecting<'_>;
}
```

The port is synchronous apart from `connect`: attaching credentials is a
lookup, never a network call. OAuth refreshes happen in the background ahead
of expiry, so a request never waits on one. Redirects are followed by `fetch`
itself, and each hop asks the gateway for its own URL. The agent hears about
the first hop whose connection needs attention.

### Inside `connectors/`

| Module | What it is |
| --- | --- |
| `config` | The TOML file and environment, checked, as plain data. Providers are still names here |
| `providers` | What each provider name means: its implementation and its skill. Produces a `Service` per connection |
| `mod` (`Connectors`) | The registry: routes each request to a connection by origin, counts outcomes, dispatches to the session |
| `api_key`, `oauth_session`, `bankid_session` | One session per kind. Each owns its state and runs its own background work (refreshing, keepalives, logins) |
| `oauth`, `bankid`, `seb` | The protocols: OAuth 2.1 with PKCE, and the BankID steps each bank implements |
| `login` | A login in progress, shared by everyone waiting on it |
| `callback` | The OAuth callback endpoint. It sees only the pending OAuth logins |
| `vault`, `browser`, `context` | Encrypted storage, scoped per connection; a cookie-keeping HTTP client; what every session gets from the process |

The composition root (`main.rs`, and `bin/connect.rs` for trying a connection from the terminal) does the wiring:
config → `providers::resolve` → open the vault, build the HTTP client →
`Connectors::start(services, deps)` → the callback server with
`connectors.oauth_logins()` → the same `Arc` as the port for `fetch`,
`connections` and metrics.

A BankID provider implements the bank's steps:

```rust
impl Seb {
    /// Start an order. The autostart token goes to the human, never to the agent.
    async fn start(&self, browser: &Browser) -> Result<(SebLogin, Secret), String>;
    async fn poll(&self, browser: &Browser, login: &mut SebLogin) -> Result<Progress, String>;
    /// Turn a completed order into a logged-in session (cookies).
    async fn finish(&self, browser: &Browser, login: SebLogin) -> Result<(), String>;
    async fn cancel(&self, browser: &Browser, login: &SebLogin);
    /// A cheap request that shows the session is alive, and keeps it so.
    fn keepalive(&self) -> Url;
    /// Whether a response says the session has ended.
    fn signed_out(&self, response: &ResponseHead) -> bool;
}
```

`BankIdProvider` is an enum over the banks for now. With a second bank it
becomes a trait with an associated order type. API keys need no flow: a header
template filled from the Secret.

The sessions do everything around those steps: PKCE and `state`, the BankID
poll loop and its expiry (`BankIdTiming`), the notices, storing results in the
vault, refreshing, keepalives, and the connection state machine:

```
not_connected ──connect──▶ logging_in ──done──▶ connected ──401/expiry──▶ expired ──connect──▶ logging_in
                               └──timeout/failure──▶ not_connected
```

An OAuth login is abandoned once nobody is waiting for it. A BankID login
runs to its order's lifetime whether or not anyone waits, so a later
`connect` can still catch it.

## What the agent sees

```js
await connections.list();
// [{ name: "google-calendar", kind: "oauth", status: "connected", origins: ["https://www.googleapis.com"] },
//  { name: "kivra", kind: "bankid", status: "expired", origins: ["https://app.kivra.com"] }]

await connections.connect("kivra", { timeout: 180_000 });  // resolves once the human approves
const inbox = await (await fetch("https://app.kivra.com/api/…")).json();  // credentials attached
```

- `connect` resolves when the login completes, and rejects on timeout or failure. Concurrent calls share one login.
- A response from a connection that needs a login carries `x-evalien-connection: <name>; status=needs-login`, so the agent knows to call `connect`.
- State changes ("waiting for BankID", "connected", "expired") are notices in the trajectory.

## Security properties

- **The agent never sees secrets.** Tokens, cookies and keys stay in the gateway and the vault. Responses are stripped of `Set-Cookie` and of headers that echo credentials. Sign-in links reach the human as notices; the model sees them as `<link sent to the human>`.
- **Credentials only go where they belong.** Each connection is attached only to requests for its listed origins, over HTTPS. A redirect to another origin is followed without credentials.
- **Logins can't be forged from outside.** The OAuth callback accepts only codes for a `state` the gateway issued, and a code is useless without the PKCE verifier, which never leaves the gateway.
- **At rest.** The vault is encrypted with a key that exists only in the Kubernetes Secret.

## Configuration

Connections live in a config file mounted from the ConfigMap; their secrets
come from the Secret by name.

```toml
[connections.google-calendar]
kind = "oauth"
provider = "google"
origins = ["https://www.googleapis.com"]
scopes = ["https://www.googleapis.com/auth/calendar.readonly"]
client_id = { secret = "GOOGLE_CLIENT_ID" }
client_secret = { secret = "GOOGLE_CLIENT_SECRET" }

[connections.kivra]
kind = "bankid"
provider = "kivra"
origins = ["https://app.kivra.com"]
keepalive_for = "1h"

[connections.example-api]
kind = "api_key"
origins = ["https://api.example.com"]
header = { name = "Authorization", value = "Bearer {key}" }
key = { secret = "EXAMPLE_API_KEY" }
```

## Build order

1. **Vault and API keys.** *Done.* The encrypted vault, the gateway port in `fetch`, origin matching, response stripping, `connections.list()`, metrics.
2. **OAuth with Google Calendar.** *Done.* The callback endpoint and its Tailscale ingress, PKCE, refresh, `connect` for OAuth.
3. **BankID.** *SEB done.* The shared flow (start, the autostart link, polling, finishing), cookie sessions in the vault, and a keepalive that lets a session go after `keepalive_for` unused. Kivra next.
4. **Later.** Proton, BankID QR, signing.
