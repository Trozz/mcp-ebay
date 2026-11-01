# mcp-ebay

A self-hosted [Model Context Protocol](https://modelcontextprotocol.io) server for [eBay](https://www.ebay.co.uk). MCP clients such as claude.ai, Claude Desktop and Claude Code can search eBay and run a seller's account: create, change, publish, end and delete listings, research prices, and post orders.

| Endpoint | What it does |
|---|---|
| `/ebay` | 23 tools and 4 prompts, acting as the eBay account that signed in |
| `/mcp` | Every enabled service's tools and prompts in one connector, named `<service>_<tool>`, for example `ebay_create_listing` |

- **People sign in with eBay itself.** The sign-in page asks the person to approve the MCP client, then sends them to eBay to sign in and grant this server's app access. The eBay refresh token is kept only in encrypted form inside their own MCP tokens: there are no API keys to paste, and the server stores nothing.
- **Listings, start to finish.** Find the category, see the item specifics and conditions it needs, then list an item in one step (inventory item, offer and publish). Change title, description, photos, item specifics, price, quantity or policies. End a listing, or delete it.
- **Listings made on the eBay website too.** Listings made on eBay itself or with other tools are listed, revised and ended through the Trading API, and can be moved over to be managed by SKU.
- **Search and research.** Search all of eBay by words, category or barcode, with price, condition, format, location and postage filters, and read any item in full.
- **Orders.** List orders awaiting shipment, read one with its delivery address, and mark it posted with tracking.
- **Safe by default.** Write tools are annotated so clients ask before running them, and the prompts show what will change and wait for agreement. `EBAY_READ_ONLY=ebay` hides and refuses every write tool. Every write is recorded in an audit log when `AUDIT_LOG_PATH` is set (tool, outcome and eBay user; never arguments or results).
- A single static Rust binary, also built as a `FROM scratch` container image. TLS end to end, a built-in OAuth 2.1 server for claude.ai custom connectors, and no database.

Built on [mcp-server-core](https://github.com/Trozz/mcp-server-core-rs), the same base as [mcp-freeagent](https://github.com/Trozz/mcp-freeagent), [mcp-companies-house](https://github.com/Trozz/mcp-companies-house), [mcp-twenty-crm](https://github.com/Trozz/mcp-twenty-crm) and [mcp-icloud](https://github.com/Trozz/mcp-icloud). Signing in through eBay uses the core's upstream OAuth sign-in (`mcp_server_core::upstream`).

## Quick start

1. **Get an eBay keyset.** Join the [eBay Developers Program](https://developer.ebay.com) and create a keyset under *Application Keys*: production for real listings, or sandbox to try things safely. Note the **App ID (Client ID)** and **Cert ID (Client Secret)**.
2. **Create a RuName.** Under *User Tokens* for the keyset, choose *Get a Token from eBay via Your Application* and add an eBay Redirect URL. Set its **auth accepted URL** to `https://<your server>/callback` (eBay needs an https address; for a laptop, use a tunnel such as `cloudflared tunnel --url http://localhost:8443`), and choose OAuth. Note the **RuName** it shows, such as `Ann_Lee-AnnLee-MyApp-abcdefgh`.
3. **Run the server:**

   ```bash
   docker build -t mcp-ebay .
   docker run -d --name mcp-ebay -p 8443:8443 \
     -e PUBLIC_URL=https://ebay-mcp.example.com \
     -e OAUTH_SIGNING_KEY="$(openssl rand -hex 32)" \
     -e EBAY_CLIENT_ID=your-app-id \
     -e EBAY_CLIENT_SECRET_FILE=/run/secrets/ebay_cert_id \
     -e EBAY_RUNAME=your-runame \
     -v /path/to/certs:/certs:ro \
     -e TLS_CERT_PATH=/certs/fullchain.pem -e TLS_KEY_PATH=/certs/privkey.pem \
     mcp-ebay
   ```

   Or behind a proxy that terminates TLS (a tunnel, nginx, Caddy): `-e INSECURE_HTTP=true -e TRUSTED_PROXIES=<proxy address>`. Locally, `cargo run -p mcp-ebay` with the same variables. Add `EBAY_ENVIRONMENT=sandbox` to use eBay's sandbox with a sandbox keyset and [sandbox test user](https://developer.ebay.com/sandbox).
4. **Connect a client.**
   - claude.ai: Settings, Connectors, add a custom connector for `https://ebay-mcp.example.com/ebay`. Claude registers itself; approve it, then sign in to eBay and agree.
   - Claude Code: `claude mcp add --transport http ebay https://ebay-mcp.example.com/ebay`, then run `/mcp`, choose ebay and authenticate.

Keep `OAUTH_SIGNING_KEY` stable and secret: it signs every token and seals every eBay refresh token, and changing it signs everyone out. eBay refresh tokens last 18 months; an MCP sign-in lasts up to 90 days, after which the client asks the person to sign in again.

## Before the first listing

eBay needs three things before an item can be listed through its Inventory API, and `get_seller_setup` checks them all:

1. **Business policies.** The account must use business policies (shipping, payment and returns). `opt_in_business_policies` opts it in; eBay then turns the account's existing settings into policies, which can take a few hours. Create or edit policies in Seller Hub (Account, Business policies), or with `api_request`.
2. **One of each policy** on the marketplace. When there is exactly one, or one marked as the default, `create_listing` uses it; otherwise pass its id.
3. **An inventory location**, the address items ship from: `create_inventory_location`.

## Tools

| Area | Tools |
|---|---|
| Account | `get_seller_setup` (start here), `opt_in_business_policies`, `create_inventory_location` |
| Categories | `suggest_categories`, `get_category_requirements` (required item specifics and allowed conditions) |
| Listing by SKU | `create_listing` (item, offer and publish in one step), `list_inventory_items`, `get_listing`, `update_listing`, `update_price_quantity` (up to 25 at once), `publish_listing`, `delete_listing`, `migrate_listings` |
| Any listing | `list_active_listings` (live, unsold or scheduled), `revise_listing` (title, description, price, quantity, SKU), `end_listing` (by SKU, offer or item number) |
| Search | `search_items`, `get_item` |
| Orders | `list_orders`, `get_order`, `mark_order_shipped` |
| Anything else | `api_get`, `api_request` (any endpoint below `sell/`, `buy/`, `commerce/` or `developer/`) |

**Two kinds of listing.** eBay keeps listings made through its Inventory API (by this server's `create_listing`, by SKU) apart from listings made on the eBay website or in older tools (by item number). `list_active_listings` shows both kinds. Change and end website listings with `revise_listing` and `end_listing` (item number), or move them over with `migrate_listings` (each needs a SKU first, which `revise_listing` can set) and then manage them by SKU. eBay does not let website tools edit listings made through the Inventory API, and the other way round.

**Ending or deleting.** `end_listing` stops a listing: by SKU it withdraws the offer and keeps the item and offer to publish again; by item number it ends the listing with a reason. `delete_listing` removes the inventory item and its offers for good, ending any live listing.

**Photos** are given as public https URLs; eBay copies them when the listing is published.

## Prompts

| Prompt | What it does |
|---|---|
| `list_item` | From a description of what you are selling: checks the account, finds the category and item specifics, researches a price, drafts the listing, and lists it once you agree. |
| `price_check` | What an item is selling for now, by condition and format, with a suggested price; or how your own listing compares. |
| `ship_orders` | A packing list of orders awaiting shipment, and marks them posted with your tracking numbers. |
| `review_listings` | What is selling, what is stale, weak titles and prices out of line, with changes to make. |

## Configuration

The eBay service reads:

| Variable | Default | Meaning |
|---|---|---|
| `EBAY_CLIENT_ID` | required | The keyset's App ID (Client ID). |
| `EBAY_CLIENT_SECRET` | required | The keyset's Cert ID (Client Secret). `EBAY_CLIENT_SECRET_FILE` reads it from a file. |
| `EBAY_RUNAME` | required | The RuName (eBay Redirect URL name) whose auth accepted URL is `PUBLIC_URL/callback`. |
| `EBAY_ENVIRONMENT` | `production` | `production` or `sandbox`. Changing it, the keyset or the scopes signs everyone out. |
| `EBAY_MARKETPLACE` | `EBAY_GB` | The marketplace tools use when none is given: `EBAY_GB`, `EBAY_US`, `EBAY_DE`, `EBAY_AU`, `EBAY_CA`, `EBAY_FR`, `EBAY_IT`, `EBAY_ES`, `EBAY_IE`, `EBAY_AT` or `EBAY_NL`. Every tool also takes a `marketplace`. |
| `EBAY_API_URL`, `EBAY_IDENTITY_URL`, `EBAY_AUTH_URL` | eBay's | Overrides for tests or a proxy. |
| `EBAY_SERVICES` | all | Comma-separated services to run (only `ebay` today). |
| `EBAY_COMBINED` | `true` | Serve every service's tools together at `/mcp`. |
| `EBAY_READ_ONLY` | none | `ebay` or `all`: hide and refuse every write tool. |

The server itself reads the [mcp-server-core variables](https://github.com/Trozz/mcp-server-core-rs#configuration): `PUBLIC_URL` and `OAUTH_SIGNING_KEY` (required), `BIND_ADDR`, `TLS_CERT_PATH`/`TLS_KEY_PATH`, `INSECURE_HTTP`, `TRUSTED_PROXIES`, `AUDIT_LOG_PATH`, `LOG_LEVEL`, `LOG_FORMAT`, `METRICS_ADDR` and `OTEL_EXPORTER_OTLP_ENDPOINT`. `MCP_AUTH_TOKEN` is refused: everyone signs in with their own eBay account.

### What the server asks eBay for

People grant these OAuth scopes when they sign in:

- `https://api.ebay.com/oauth/api_scope`
- `https://api.ebay.com/oauth/api_scope/sell.inventory`: inventory items, offers, locations
- `https://api.ebay.com/oauth/api_scope/sell.account`: business policies and programmes
- `https://api.ebay.com/oauth/api_scope/sell.fulfillment`: orders and shipping
- `https://api.ebay.com/oauth/api_scope/commerce.identity.readonly`: the account's username

Searching and categories use an application token (client credentials) of the server's own keyset. eBay's daily call limits apply per keyset; `api_get` with `developer/analytics/v1_beta/rate_limit` shows them.

## How it works

```
crates/
  mcp-ebay/              the binary: one mcp-server-core Server with the eBay service
  mcp-ebay-marketplace/  the eBay service
    config.rs     EBAY_* settings and the marketplaces
    oauth.rs      eBay OAuth: sign-in URL, code exchange, refresh, application token, token cache
    verifier.rs   signing in through eBay (mcp_server_core::upstream)
    api.rs        REST and Trading API calls: retries, a fresh token after a 401, error mapping
    xml.rs        the little XML the Trading API needs
    tools/        the tools, by area
    prompts.rs    the prompts
```

User access tokens (two hours) are cached in memory per person and refreshed from the sealed refresh token when they run out; eBay does not replace refresh tokens, so nothing needs storing. Reads are retried once after a network error or a 5xx; writes never are, so a slow create cannot run twice.

## Development

```bash
cargo test --workspace          # unit tests, and the tools and sign-in against a fake eBay
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

The integration tests run the tools and the whole OAuth sign-in against an in-memory eBay (`crates/mcp-ebay-marketplace/tests/support/fake_ebay.rs`), so they need no network or keyset.

## Licence

MIT
