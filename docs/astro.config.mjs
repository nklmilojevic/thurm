import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import catppuccin from '@catppuccin/starlight';
import starlightLinksValidator from 'starlight-links-validator';

export default defineConfig({
  site: 'https://nklmilojevic.github.io',
  base: '/thurm',
  trailingSlash: 'always',
  integrations: [
    starlight({
      title: 'Thurm',
      description: 'A native macOS terminal with persistent sessions and coding agent controls.',
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/nklmilojevic/thurm' },
      ],
      editLink: {
        baseUrl: 'https://github.com/nklmilojevic/thurm/edit/main/docs/',
      },
      lastUpdated: true,
      customCss: [
        '@fontsource-variable/inter',
        '@fontsource-variable/jetbrains-mono',
        './src/styles/custom.css',
      ],
      expressiveCode: {
        themes: ['catppuccin-mocha', 'catppuccin-latte'],
      },
      sidebar: [
        {
          label: 'Get started',
          items: [
            { label: 'Overview', slug: 'index' },
            'install',
            'usage',
          ],
        },
        {
          label: 'Configure',
          items: ['configuration', 'configuration-reference'],
        },
        {
          label: 'Guides',
          items: ['cli', 'agents', 'sessions'],
        },
        {
          label: 'Help',
          items: ['troubleshooting', 'development'],
        },
      ],
      plugins: [
        catppuccin({
          dark: { flavor: 'mocha', accent: 'mauve' },
          light: { flavor: 'latte', accent: 'mauve' },
        }),
        starlightLinksValidator(),
      ],
    }),
  ],
});
