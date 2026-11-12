# piqueld.dev

The marketing site for piqueld, served at <https://piqueld.dev>. It is plain
static HTML and CSS, with a few lines of inline script that copy the install
command on a click, and no build step or server runtime, so any static
file server that follows symlinks can serve this directory as it is. The
`site` service of `deploy/piqueld-dev.toml` serves it with
`deploy/site/Dockerfile`; see
[publishing piqueld.dev](../../docs/dev/publishing.md).

`index.html` is the whole site: a change's path through piqueld, drawn as a
transit map, followed by what sets piqueld apart. `favicon.svg` and
`favicon.png`, piqueld's logo, link to the dashboard's in
`apps/piqueld-ui/assets`, and the image copies them from there.
`install.sh`, served at <https://piqueld.dev/install.sh>, installs piqueld
from its latest GitHub release; the `install` job of the
[release checks](https://github.com/sourceorigin-dev/piqueld/blob/main/docs/dev/validation.md#release-checks)
runs it against binaries built from `main`. `fonts/overpass.woff2` is the Latin subset of Overpass, an OFL-licensed font,
served from this site so that the page makes no third-party requests; its
licence is in `fonts/OFL.txt`.

## Preview

```console
python3 -m http.server --directory apps/site 8000
```

Then open <http://localhost:8000>. To see it served as it is published, with
the book next to it, deploy your worktree to a preview of `piqueld-dev`; see
[previewing a branch](../../docs/dev/publishing.md#preview-a-branch).
