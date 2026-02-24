import { defineConfig } from 'vitepress'

// ── Replace these per-project ──────────────────────────────────────────────
const projectName = 'Nidhogg'
const projectDescription = 'Shortbread vector tiles from OpenStreetMap'
const githubUrl = 'https://github.com/user/nidhogg'
const base = '/' // set to '/repo-name/' for GitHub Pages project sites
// ────────────────────────────────────────────────────────────────────────────

export default defineConfig({
  title: projectName,
  description: projectDescription,
  base,

  appearance: 'dark',

  head: [
    ['link', { rel: 'icon', type: 'image/svg+xml', href: `${base}favicon.svg` }],
  ],

  themeConfig: {
    logo: { light: '/nidhogg-logo.svg', dark: '/nidhogg-logo-dark.svg' },

    nav: [
      { text: 'Guide', link: '/guide/' },
      { text: 'API Docs', link: '/api/' },
    ],

    socialLinks: [
      { icon: 'github', link: githubUrl },
    ],

    footer: {
      message: `Released under the MIT License.`,
    },

    sidebar: {
      '/guide/': [
        {
          text: 'Guide',
          items: [
            { text: 'Getting Started', link: '/guide/' },
            { text: 'Installation', link: '/guide/install' },
            { text: 'Configuration', link: '/guide/configuration' },
            { text: 'Usage', link: '/guide/usage' },
            { text: 'Advanced', link: '/guide/advanced' },
          ],
        },
      ],
      '/api/': [
        {
          text: 'API Reference',
          items: [
            { text: 'Overview', link: '/api/' },
            { text: 'nidhogg::config', link: '/api/config' },
            { text: 'nidhogg::tile', link: '/api/tile' },
            { text: 'nidhogg::pipeline', link: '/api/pipeline' },
          ],
        },
      ],
    },
  },
})
