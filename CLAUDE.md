# gh-template

VitePress documentation site template with custom typography and per-project color themes.

## Commands

```sh
pnpm install    # install dependencies
pnpm dev        # dev server at localhost:5173
pnpm build      # production build
pnpm preview    # preview production build
```

## Structure

- `docs/.vitepress/config.ts` — project name, description, GitHub URL, logo, sidebar
- `docs/.vitepress/theme/style.css` — active color theme (copy from a `colors-*.css` file to switch)
- `docs/.vitepress/theme/colors-*.css` — saved color palettes (nidhogg, elivagar, pbfhogg, kvakk, default)
- `docs/index.md` — home page (hero, features, body content)
- `docs/public/` — logos, icons, fonts
- `raw/` — source SVGs, example READMEs, reference material

## Switching themes

Copy the contents of a `colors-*.css` file into the `:root` and `.dark` blocks in `style.css`. Also update:
- `config.ts` — `projectName`, `projectDescription`, `githubUrl`, `logo`
- `docs/index.md` — hero name, text, tagline, image, GitHub link

## Current active theme

Pbfhogg (Blood & Iron). Saved palettes are in `colors-*.css` files.
