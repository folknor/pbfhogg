# mise.jdx.dev VitePress Reference

Source: https://github.com/jdx/mise
Site: https://mise.jdx.dev/

Same approach as us — extends default VitePress theme, no third-party theme package.

## Key files

- `docs/.vitepress/theme/index.ts` — theme entry, extends DefaultTheme, adds tabs plugin
- `docs/.vitepress/theme/custom.css` — ~600 lines of CSS overrides (the bulk of their customization)
- `docs/.vitepress/theme/HomeHero.vue` — custom hero with animated gradient orbs, stats grid
- `docs/.vitepress/theme/MiseLogo.vue` — animated SVG logo with hover effects
- `docs/.vitepress/config.ts` — VitePress config, Algolia search, Google Fonts in head
- `docs/.vitepress/cli_commands.ts` — auto-generated CLI reference for sidebar
- `docs/.vitepress/stars.data.ts` — GitHub star count data loader
- `docs/.vitepress/grammars/` — custom TextMate grammars for KDL and mise-toml syntax highlighting

## Fonts (Google Fonts, loaded in config.ts head)

- **Bebas Neue** — h1, nav, hero (display font, like our Jomhuria)
- **Inter** — body text (--vp-font-family-base)
- **JetBrains Mono** — code (--vp-font-family-mono)

## CSS highlights (custom.css)

- Brand colors: cyan/teal primary (#00d9ff)
- Deep dark mode: #0a0a0b, #121214 backgrounds
- Nav: backdrop blur, logo hover glow, gradient text shimmer on title hover
- Sidebar: blur backdrop, hover slide effect, active left border accent
- Code blocks: rounded corners, box shadows, hover effects, styled language labels
- Feature cards: hover lift with shadow, gradient backgrounds
- Links: animated underline (scaleX transform)
- Tables: rounded corners, hover row highlighting
- Page transitions: fadeInUp animation on content
- Custom scrollbar styling
- Badge styles: .badge-new, .badge-beta, .badge-deprecated
- Terminal-style class with fake window dots

## Plugins (from package.json)

- vitepress 1.6.4
- vitepress-plugin-group-icons — icons next to code group tabs
- vitepress-plugin-mermaid — mermaid diagram support
- vitepress-plugin-tabs — tabbed content in markdown

## What they do that we don't (yet)

- Custom Vue hero component with animations
- Algolia DocSearch
- Page transition animations
- Code block hover effects and styled language labels
- Animated underlines on links
- Custom scrollbar
- Custom TextMate grammars for syntax highlighting
- Star count badge via data loader + DOM injection

## What we do that they don't

- Fluid 4K scaling with clamp()
- Full px-to-rem conversion
- Sticky nav on sidebar pages
- Wider doc layout for large screens
- Multiple color themes (they have one brand)
- Light/dark logo variants
