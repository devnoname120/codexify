# Codexify notarization webhook relay

This Worker converts Apple's unauthenticated notarization callback into an authenticated GitHub `repository_dispatch`. It is only a wake-up relay: the GitHub finalizer independently queries Apple and verifies the draft release before publishing it.

## 1. Create the GitHub token

Create a fine-grained personal access token with these exact settings:

- **Token name:** `Codexify Notary Webhook`
- **Resource owner:** `devnoname120`
- **Repository access:** **Only select repositories** → `codexify`
- **Repository permissions:** **Contents: Read and write**
- **Every other permission:** **No access**

Copy the token when GitHub displays it. It will be entered once as a Cloudflare secret.

## 2. Deploy the Worker

From this directory:

```bash
cp wrangler.toml.example wrangler.toml
npx wrangler login
npx wrangler secret put GITHUB_TOKEN
npx wrangler secret put WEBHOOK_SECRET
npx wrangler deploy
```

For `GITHUB_TOKEN`, paste the fine-grained token from step 1.

For `WEBHOOK_SECRET`, paste the contents of the generated local secret file supplied during Codexify setup. The value must remain secret and must not be committed or entered as a plain-text Wrangler variable.

Wrangler prints a URL shaped like:

```text
https://codexify-notary-webhook.<your-subdomain>.workers.dev
```

The complete Apple callback URL is:

```text
https://codexify-notary-webhook.<your-subdomain>.workers.dev/apple-notary/<WEBHOOK_SECRET>
```

Send that complete URL back to the Codexify setup process so it can set the GitHub Actions secret `APPLE_NOTARY_WEBHOOK_URL`.

## 3. Verify the relay

After the GitHub secret has been configured, send one harmless wake-up:

```bash
curl -q -i -X POST -H 'content-type: application/json' --data '{}' \
  'https://codexify-notary-webhook.<your-subdomain>.workers.dev/apple-notary/<WEBHOOK_SECRET>'
```

Expected response:

```text
HTTP/2 202
Accepted
```

This starts the finalizer workflow, which exits successfully without changing anything when no notarization-bearing draft exists.

## Secrets and trust boundary

The Worker contains only:

- `GITHUB_TOKEN`, stored as a Cloudflare secret;
- `WEBHOOK_SECRET`, stored as a Cloudflare secret;
- `GITHUB_REPOSITORY=devnoname120/codexify`, stored as a non-secret variable.

Do not put the Developer ID certificate, certificate password, App Store Connect API key, or Apple Team ID in Cloudflare. The callback body is bounded and discarded; its contents are never trusted or forwarded.
