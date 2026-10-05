import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import catppuccin from '@catppuccin/starlight';
import starlightLinksValidator from 'starlight-links-validator';

export default defineConfig({
  site: 'https://docs.thurm.rs',
  trailingSlash: 'always',
  integrations: [
    starlight({
      title: 'Thurm',
      description: 'A native macOS terminal with persistent sessions and coding agent controls.',
      logo: { src: './src/assets/icon.png', alt: '' },
      favicon: '/favicon.png',
      social: [
        { icon: 'external', label: 'thurm.rs', href: 'https://thurm.rs/' },
        { icon: 'github', label: 'GitHub', href: 'https://github.com/nklmilojevic/thurm' },
      ],
      components: {
        ThemeProvider: './src/components/ThemeProvider.astro',
        ThemeSelect: './src/components/ThemeSelect.astro',
      },
      editLink: {
        baseUrl: 'https://github.com/nklmilojevic/thurm/edit/main/docs/',
      },
      lastUpdated: true,
      customCss: [
        '@fontsource-variable/geist',
        '@fontsource-variable/jetbrains-mono',
        './src/styles/custom.css',
      ],
      expressiveCode: {
        themes: ['catppuccin-mocha'],
        styleOverrides: {
          borderRadius: '0.75rem',
          borderColor: '#313244',
          codeBackground: '#181825',
          codeFontFamily: "'TX-02', 'JetBrains Mono Variable', ui-monospace, monospace",
          codeFontSize: '0.9rem',
          uiFontFamily: "'Geist Variable', ui-sans-serif, system-ui, sans-serif",
          frames: {
            editorBackground: '#181825',
            terminalBackground: '#181825',
            editorTabBarBackground: '#181825',
            editorActiveTabBackground: '#181825',
            editorActiveTabIndicatorTopColor: 'transparent',
            editorTabBarBorderBottomColor: '#313244',
            terminalTitlebarBackground: '#181825',
            terminalTitlebarBorderBottomColor: '#313244',
            frameBoxShadowCssValue: 'none',
          },
        },
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
          items: ['cli', 'agents', 'agent-prompts', 'agent-integration', 'sessions', 'remote', 'attach', 'workflows'],
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
