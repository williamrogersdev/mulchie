// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLlmsTxt from 'starlight-llms-txt';

const site = 'https://mulchie.com';

export default defineConfig({
  site,
  // One policy for every page: directory URLs with a trailing slash.
  trailingSlash: 'always',
  build: { format: 'directory' },
  devToolbar: { enabled: false },
  // Landing and install pages ship no client JS, so no link prefetching.
  prefetch: false,
  // Keep small font subsets as files so the CSP can stay font-src 'self'.
  vite: { build: { assetsInlineLimit: 0 } },
  integrations: [
    starlight({
      title: 'Mulchie',
      disable404Route: true,
      description: 'Documentation for Mulchie, the supervisor for machines that run many AI coding agents.',
      logo: { src: './src/assets/mark.png', alt: '' },
      favicon: '/favicon-32.png',
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/williamrogersdev/mulchie' }],
      editLink: { baseUrl: 'https://github.com/williamrogersdev/mulchie/edit/main/site/' },
      customCss: ['./src/styles/tokens.css', './src/styles/docs.css'],
      head: [
        { tag: 'meta', attrs: { property: 'og:image', content: `${site}/og/default.png` } },
        { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
        { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: `${site}/og/default.png` } },
        { tag: 'link', attrs: { rel: 'icon', href: '/favicon.ico', sizes: '48x48' } },
        { tag: 'link', attrs: { rel: 'apple-touch-icon', href: '/apple-touch-icon.png' } },
        { tag: 'link', attrs: { rel: 'manifest', href: '/site.webmanifest' } },
      ],
      sidebar: [
        { label: 'Start', items: [{ slug: 'docs' }, { slug: 'docs/getting-started' }, { label: 'Install', link: '/install/' }] },
        {
          label: 'Supervise agents',
          items: [{ slug: 'docs/sessions' }, { slug: 'docs/hooks' }, { slug: 'docs/updates' }],
        },
        {
          label: 'Reference',
          items: [
            { slug: 'docs/cli' },
            { slug: 'docs/mcp' },
            { slug: 'docs/http-api' },
            { slug: 'docs/configuration' },
            { slug: 'docs/watch' },
          ],
        },
        {
          label: 'Guides',
          items: [{ slug: 'docs/safety' }, { slug: 'docs/headless' }, { slug: 'docs/troubleshooting' }, { slug: 'docs/faq' }],
        },
      ],
      plugins: [
        starlightLlmsTxt({
          projectName: 'Mulchie',
          description:
            'Mulchie is an open source (MIT) macOS menu bar app, CLI, local HTTP API and MCP server that keeps one machine healthy while many AI coding agents run on it. It is a supervisor, not a sandbox and not an antivirus.',
          details:
            'Core and CLI also run headless on Linux. Source: https://github.com/williamrogersdev/mulchie. Install: https://mulchie.com/install/.',
          optionalLinks: [
            { label: 'Install guide', url: `${site}/install/`, description: 'Every install path, verification, updating and uninstalling.' },
            { label: 'Source code', url: 'https://github.com/williamrogersdev/mulchie', description: 'GitHub repository.' },
          ],
        }),
      ],
    }),
  ],
});
