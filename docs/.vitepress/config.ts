import { defineConfig } from 'vitepress'

// ── Replace these per-project ──────────────────────────────────────────────
const projectName = 'pbfhogg'
const projectDescription = 'Fast OpenStreetMap PBF reader and writer for Rust'
const githubUrl = 'https://github.com/folknor/pbfhogg'
const base = '/pbfhogg/'
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
    logo: '/pbfhogg-logo.svg',

    nav: [
      { text: 'Guide', link: '/guide/' },
      { text: 'CLI', link: '/cli/' },
      { text: 'API Docs', link: 'https://docs.rs/pbfhogg' },
    ],

    search: {
      provider: 'local',
    },

    socialLinks: [
      { icon: 'github', link: githubUrl },
    ],

    footer: {
      message: `Released under the Apache License 2.0. | Copyright folk@folk.wtf`,
    },

    sidebar: {
      '/guide/': [
        {
          text: 'Guide',
          items: [
            { text: 'Getting Started', link: '/guide/' },
            { text: 'Reading PBF Files', link: '/guide/reading' },
            { text: 'Writing PBF Files', link: '/guide/writing' },
            { text: 'Indexdata', link: '/guide/indexdata' },
            { text: 'Performance', link: '/guide/performance' },
          ],
        },
        {
          text: 'Reference',
          items: [
            { text: 'Correctness', link: '/guide/correctness' },
          ],
        },
      ],
      '/cli/': [
        {
          text: 'CLI Reference',
          items: [
            { text: 'Overview', link: '/cli/' },
            { text: 'Commands', link: '/cli/commands' },
          ],
        },
        {
          text: 'Compatibility',
          items: [
            { text: 'Osmium Parity', link: '/cli/osmium-parity' },
            { text: 'Deviations', link: '/cli/deviations' },
          ],
        },
      ],
    },
  },
})
