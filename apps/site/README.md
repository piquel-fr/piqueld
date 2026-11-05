# piqueld.dev

The marketing site for piqueld, served at <https://piqueld.dev>. It is plain
static HTML and CSS with no build step and no server runtime, so any static
file server can serve this directory as it is. The `site` service of
`deploy/piqueld-dev.toml` serves it with `deploy/site/Dockerfile`; see
[publishing piqueld.dev](../../docs/dev/publishing.md).

`index.html` is the whole site: a change's path through piqueld, drawn as a
transit map, followed by what sets piqueld apart. `favicon.svg` and `favicon.png` are piqueld's
logo, copied from `apps/piqueld-ui/assets`. `fonts/overpass.woff2` is the
Latin subset of Overpass, an OFL-licensed font, served from this site so that
the page makes no third-party requests; its licence is in `fonts/OFL.txt`.

## Preview

```console
python3 -m http.server --directory apps/site 8000
```

Then open <http://localhost:8000>.
