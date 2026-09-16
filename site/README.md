# rthas docs

VitePress site, same shape as [Arthas docs](https://arthas.aliyun.com/en/doc/): left sidebar, right table of contents, local search.

```bash
cd site
npm install
npm run docs:dev      # http://localhost:5173/rthas/
npm run docs:build
npm run docs:preview
```

Published at **https://xuqianjin-stars.github.io/rthas/** (GitHub project Pages). That URL lives next to the user site at [xuqianjin-stars.github.io](https://xuqianjin-stars.github.io/); they are different repos and do not overwrite each other.

`.github/workflows/pages.yml` deploys on push to `main`. Enable **Settings → Pages → Source: GitHub Actions** once.
