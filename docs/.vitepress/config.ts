import { defineConfig } from 'vitepress'

// ── Replace these per-project ──────────────────────────────────────────────
const projectName = 'Nidhogg'
const projectDescription = 'A project description goes here'
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
    logo: '/nidhogg-logo.svg',

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

    // Disable sidebar on home page, enable on guide pages
    sidebar: {
      '/guide/': [
        {
          text: 'Guide',
          items: [
            { text: 'Getting Started', link: '/guide/' },
            { text: 'Installation', link: '/guide/install' },
          ],
        },
      ],
    },
  },
})
