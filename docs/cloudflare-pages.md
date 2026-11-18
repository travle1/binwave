# Cloudflare Pages deployment

The app uses the separate `binwave-app` project. Never deploy to the `binwave`
project: `binwave.pages.dev` is the marketing site.

Repository files and Git history may be public. Only `dist/web` is deployed: the browser app,
JavaScript and WASM runtime, bundled example decoders, and the tiny demo.
The resulting website and those downloadable assets are public. User-opened
data files remain on their device.

Build and validate before deploying:

```sh
npm ci
node tests/browser_python.mjs
trunk build --release --locked
node --experimental-default-type=module tests/browser_decoder.mjs
```

After authorizing publication, create the Pages project once:

```sh
npx wrangler pages project create binwave-app --production-branch main
```

Publish the built assets:

```sh
npx wrangler pages deploy dist/web --project-name binwave-app --branch main
```

This uses Direct Upload; no GitHub repository connection is needed. Future
builds can also deploy through CI with scoped Cloudflare credentials. A Direct
Upload project cannot later switch to Cloudflare's built-in Git integration.
Never upload the repository root or `samples/data`.

See [Cloudflare Direct Upload](https://developers.cloudflare.com/pages/get-started/direct-upload/).

## Marketing verification binding

The marketing project uses the `TURNSTILE_VERIFY` service binding declared in
`marketing/wrangler.jsonc` to call its existing verification Worker. Deploy the
marketing Functions with that configuration so the binding is available. Missing
or failed verification refuses the signup instead of bypassing the check.
Account-specific hostnames, personal email addresses and local paths belong
outside tracked source files. Use a GitHub noreply address for commit metadata.
