# ra-jit site

The SvelteKit app at https://ra-jit.ix.dev, a Cloudflare Worker (`@sveltejs/adapter-cloudflare`). The home page is
prerendered; `/updates` is server-rendered and stores signups in the D1 database `ra-jit-updates` (binding `DB`).

```
bun install
bun run dev      # local, with a local D1 (apply the migration with --local first)
bun run build
bun run deploy   # build + wrangler deploy
```

## Signups (`/updates`)

One row per email in `signups` (email lowercased; a repeat signup updates `company` and `why`, keeps the first-touch
metadata). Metadata: `request.cf` country, region, city, asn, asOrganization, timezone; user-agent; referer (the page
before `/updates`); accept-language; `src` and `utm_*` from the `/updates` URL (the README button uses `?src=readme`).
No IPs are stored. Rate limit: 3 posts per email and 60 overall per minute, logged in `signup_attempts`.

Turnstile: set `TURNSTILE_SITE_KEY` in `wrangler.jsonc` `vars` and the secret with `wrangler secret put`. With no
secret, verification is skipped; with no site key, the widget is not shown (dev).

### One-time setup

```
npx wrangler d1 create ra-jit-updates      # paste the printed database_id into wrangler.jsonc (TODO there)
npx wrangler d1 migrations apply ra-jit-updates --remote
npx wrangler secret put TURNSTILE_SECRET_KEY
bun run deploy
```

Local dev: `npx wrangler d1 migrations apply ra-jit-updates --local`.

### Reading signups

```
npx wrangler d1 execute ra-jit-updates --remote --command \
  "select email, company, why, src, country, city, as_organization, created_at from signups order by created_at desc"
npx wrangler d1 execute ra-jit-updates --remote --command "select count(*) from signups"
```

Add `--json` for machine-readable output.
