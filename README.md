# gh-template

A VitePress documentation template with custom typography (Jomhuria + EB Garamond), fluid 4K scaling, and pre-built color themes.

## Themes

| Theme | Brand | File |
|---|---|---|
| Default | (blank template) | `docs/.vitepress/theme/colors-default.css` |
| Nidhogg | Wyrm's Emerald | `docs/.vitepress/theme/colors-nidhogg.css` |
| Elivagar | Icy Cyan | `docs/.vitepress/theme/colors-elivagar.css` |
| Pbfhogg | Blood Red | `docs/.vitepress/theme/colors-pbfhogg.css` |
| Kvakk | Duck Bill Orange | `docs/.vitepress/theme/colors-kvakk.css` |

## Using this template in a project

### Initial setup

From your project repo:

```sh
# Add the template as a remote
git remote add template git@github.com:user/gh-template.git

# Fetch and merge the template into your repo
git fetch template
git merge template/master --allow-unrelated-histories
```

### Customize for your project

1. **Config** — edit `docs/.vitepress/config.ts`:
   ```ts
   const projectName = 'YourProject'
   const projectDescription = 'What it does'
   const githubUrl = 'https://github.com/user/your-project'
   const base = '/your-project/'  // must match your repo name
   ```

2. **Colors** — copy the palette you want into `style.css`, or replace the color variables in the `:root` and `.dark` blocks. The saved palettes are in `docs/.vitepress/theme/colors-*.css`.

3. **Logos** — replace the SVGs in `docs/public/` with your own. Update the `logo` and hero `image` paths in `config.ts` and `docs/index.md`. If you need light/dark variants:
   ```ts
   logo: { light: '/my-logo.svg', dark: '/my-logo-dark.svg' }
   ```

4. **Content** — replace the placeholder docs in `docs/guide/` and `docs/api/` with your actual documentation. Update the sidebar entries in `config.ts` to match.

### Pre-publish checklist

Before deploying, make sure you've updated everything that ships with template placeholder content:

**`docs/.vitepress/config.ts`:**
- [ ] `projectName` — template placeholder
- [ ] `projectDescription` — template placeholder
- [ ] `githubUrl` — powers the GitHub icon in the header
- [ ] `base` — set to `'/repo-name/'` for GitHub Pages project sites
- [ ] `logo` — update path to your logo SVG
- [ ] `footer.message` — update to your actual license
- [ ] Sidebar sections — placeholder Guide/API structure, update to match your actual pages

**`docs/index.md`:**
- [ ] `hero.name`, `hero.text`, `hero.tagline` — template placeholders
- [ ] `hero.image.src` — update path to your logo SVG
- [ ] Hero GitHub button `link` — update to your repo URL
- [ ] Feature cards — template placeholders (icons, titles, descriptions)
- [ ] Body content below the fold — template placeholder

**`docs/guide/index.md`:**
- [ ] Placeholder content — replace with your own getting started guide

**`docs/public/`:**
- [ ] Replace logo SVGs with your own
- [ ] Add a `favicon.svg` (referenced in config but not present in the template)

5. **README** — `raw/` contains ready-made README drafts for each project (`nidhogg-README.md`, `elivagar-README.md`, `pbfhogg-README.md`) with logos, badges, and light/dark mode already wired up. To use one as a starting point, copy it to your project root as `README.md` and adjust paths.

   Each README header uses a `<picture>` tag so GitHub shows the right logo for light/dark mode:
   ```html
   <p align="center">
     <picture>
       <source media="(prefers-color-scheme: dark)" srcset="my-logo-text-dark.svg">
       <img src="my-logo-text.svg" width="300" alt="MyProject">
     </picture>
     <br>
     <em>Project tagline here</em>
   </p>
   ```

   **Creating logo SVGs:** Export your logo-text from Inkscape with text converted to paths. For the dark variant, replace the dark fills with your theme's dark-mode brand-1 color:

   | Theme | Light fill | Dark fill |
   |---|---|---|
   | Nidhogg | `#043927` | `#22d974` |
   | Elivagar | `#1a2a3a` | `#33d4f0` |
   | Pbfhogg | `#1b2e21` | `#cc3030` |
   | Kvakk | `#d07810` | `#f0a040` |

   These colors come from `--vp-c-brand-1` in the `.dark` block of each `colors-*.css` file. Kvakk's `#d07810` orange works on both light and dark backgrounds, so it doesn't need a separate dark variant.

   **Badges** use [shields.io](https://shields.io). Common ones for Rust projects:
   ```html
   <a href="https://crates.io/crates/mycrate"><img src="https://img.shields.io/crates/v/mycrate" alt="crates.io"></a>
   <a href="https://docs.rs/mycrate"><img src="https://img.shields.io/docsrs/mycrate" alt="docs.rs"></a>
   <img src="https://img.shields.io/badge/rust-stable-orange?logo=rust" alt="Rust">
   <img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License">
   ```

### Pulling template updates

When the template gets improvements (layout fixes, font tweaks, new features):

```sh
git fetch template
git merge template/master
```

Resolve conflicts in the files you've customized (config, colors, content). The structural CSS and layout changes will merge cleanly.

### Deploy to GitHub Pages

Add `.github/workflows/docs.yml`:

```yaml
name: Deploy docs
on:
  push:
    branches: [main]

permissions:
  pages: write
  id-token: write

jobs:
  deploy:
    runs-on: ubuntu-latest
    environment:
      name: github-pages
      url: ${{ steps.deployment.outputs.page_url }}
    steps:
      - uses: actions/checkout@v4
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 22
          cache: pnpm
      - run: pnpm install
      - run: pnpm build
      - uses: actions/upload-pages-artifact@v3
        with:
          path: docs/.vitepress/dist
      - id: deployment
        uses: actions/deploy-pages@v4
```

Then in your repo settings, enable Pages with source set to "GitHub Actions".

Your site will be at `https://username.github.io/repo-name/`.

## Local development

```sh
pnpm install
pnpm dev        # http://localhost:5173
pnpm build      # production build
pnpm preview    # preview production build
```

## Structure

```
docs/
  .vitepress/
    theme/
      style.css              # main theme (colors, typography, layout overrides)
      colors-default.css     # blank template for new themes
      colors-nidhogg.css     # saved palette: wyrm's emerald
      colors-elivagar.css    # saved palette: icy cyan
      colors-pbfhogg.css     # saved palette: blood red
      colors-kvakk.css       # saved palette: duck bill orange (catppuccin)
      index.ts               # theme entry point
    config.ts                # VitePress config (nav, sidebar, project info)
  public/
    fonts/                   # Jomhuria, EB Garamond, Almendra
    icons/                   # feature card icons (Fluent Color)
    *.svg                    # project logos
  guide/                     # guide pages
  api/                       # API reference pages
  index.md                   # home page
raw/
  *-logo.svg                 # icon-only logos
  *-logo-text.svg            # logo + project name (light mode)
  *-logo-text-dark.svg       # logo + project name (dark mode, not needed for kvakk)
  *-README.md                # ready-made README drafts with logos and badges
```
