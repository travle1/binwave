# Cloudflare Pages deployment

The app uses the separate `binwave-app` project. Never deploy to the `binwave`
project: `binwave.pages.dev` is the marketing site.

The repository stays private. Only `dist/web` is published: the browser app,
JavaScript and WASM runtime, bundled example decoders, and the tiny demo.
The resulting website and those downloadable assets are public. User-opened
data files remain on their device.

Build and validate before deploying:

```sh
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
